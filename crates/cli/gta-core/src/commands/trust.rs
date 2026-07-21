//! `gta trust` — manage the repository's trust root (the signed `refs/gitana/trust` chain). `init`
//! bootstraps a self-signed root; `add-key`/`remove-key` and `set-policy` extend the chain with new
//! signed updates; `list` shows the current policy and enrolled key fingerprints; `sync` safely
//! adopts the origin's trust root (forward-only, only if it verifies).

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::{
	TRUST_REF, TrustSyncOutcome, trust_add_key, trust_init, trust_list, trust_remove_key,
	trust_set_policy, trust_sync,
};
use gitana_remote::{self as transport, HttpTransport, Origin};
use gitana_repository::Repository;
use gitana_trust::{AuditEvent, KeyId, Policy, TrustRoot, TrustedKey};

use crate::Backend;
use crate::dispatch::{self, RepoCommand};
use crate::identity::CliIdentity;
use crate::repo;
use crate::signer::{self, CliSigner};
use crate::{git_config, transport_for, url_rewrite};

/// A `trust` sub-command.
pub enum Action {
	/// Bootstrap the trust root, enrolling the signing key under `policy`.
	Init {
		policy: String,
		signing_key: Option<PathBuf>,
		break_glass: bool,
		/// Report what bootstrapping would do, without writing anything.
		dry_run: bool,
	},
	/// Show the current policy and enrolled key fingerprints.
	List,
	/// Enrol a public key (a file path or a literal OpenSSH line / armored OpenPGP certificate).
	AddKey {
		key: String,
		signing_key: Option<PathBuf>,
	},
	/// Remove a key by fingerprint (`SHA256:…` or OpenPGP hex) or public-key line/file.
	RemoveKey {
		key: String,
		signing_key: Option<PathBuf>,
		break_glass: bool,
	},
	/// Change the enforcement policy.
	SetPolicy {
		policy: String,
		signing_key: Option<PathBuf>,
		break_glass: bool,
		/// Report the cutover impact, without writing anything.
		dry_run: bool,
	},
	/// Safely adopt the origin's trust root into the local `refs/gitana/trust`. On a first-use
	/// bootstrap (local trust unset), `expect` pins the fingerprint of the key that must have signed
	/// the incoming root's bootstrap.
	Sync { expect: Option<String> },
}

/// Run a `trust` sub-command in the repository containing `cwd`.
pub async fn run(cwd: &Path, action: Action) -> Result<()> {
	// `sync` transacts with the origin, so it does its own discovery + HTTP + hash dispatch (like
	// `fetch`), rather than the offline `on_repo` path the other sub-commands share.
	if let Action::Sync { expect } = action {
		return sync(cwd, expect).await;
	}
	dispatch::on_repo(
		cwd,
		Trust {
			action,
			cwd: cwd.to_path_buf(),
		},
	)
	.await
}

/// Fetch the origin's `git-upload-pack` advertisement and adopt its `refs/gitana/trust` — after
/// verifying it as a forward-only candidate over the local root — then print the resulting root.
async fn sync(cwd: &Path, expect: Option<String>) -> Result<()> {
	let found = repo::discover(cwd).await?;
	// The origin URL is `remote.origin.url` with `url.*.insteadOf` applied, read from the merged config.
	// (`trust sync` both fetches and pushes the trust ref over this one origin; it uses fetch-direction
	// `insteadOf` rather than `pushInsteadOf`, which would only differ under a push-specific rewrite.)
	let config = git_config::effective_config_at(&found.git_dir, &found.common_dir).await?;
	let origin = url_rewrite::fetch_origin(&config, "origin")?;
	// A relative askpass resolves against the worktree root, as git runs it from there (bare: git dir).
	let askpass_cwd = found
		.worktree_root
		.clone()
		.unwrap_or_else(|| found.common_dir.clone());
	let http = transport_for(config, &origin, askpass_cwd)?;
	let body = transport::fetch_advertisement(&http, &origin, "git-upload-pack").await?;

	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	match local {
		HashKind::Sha1 => sync_into::<Sha1>(&http, &origin, &found, &body, expect).await,
		HashKind::Sha256 => sync_into::<Sha256>(&http, &origin, &found, &body, expect).await,
	}
}

async fn sync_into<H: HashAlgorithm>(
	http: &impl HttpTransport,
	origin: &Origin,
	found: &repo::RepositoryLayout,
	body: &[u8],
	expect: Option<String>,
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir).await?;
	let identity = CliIdentity::new(&repository);
	// On a first-use bootstrap, `trust_sync` asks whether to adopt the unseen root; the fast-forward
	// path never calls this. `--expect` pins the anchor for a non-interactive decision; otherwise a
	// terminal is prompted, and a non-terminal (gta-mcp, piped stdin) fails closed.
	let confirm =
		async move |root: &TrustRoot, anchor: &KeyId| confirm_adoption(&expect, root, anchor);
	match trust_sync(http, &repository, origin, body, &identity, confirm).await? {
		TrustSyncOutcome::RemoteUnset => {
			println!("{} has no trust root; nothing to sync.", origin.url);
		}
		TrustSyncOutcome::UpToDate => {
			println!("Trust root is already up to date.");
			print_root(&repository).await?;
		}
		TrustSyncOutcome::Updated { old, new, anchor } => {
			match old {
				None => println!(
					"Adopted trust root from {}; {TRUST_REF} set to {new}",
					origin.url
				),
				Some(old) => {
					println!(
						"Updated trust root from {}; {TRUST_REF} {old} -> {new}",
						origin.url
					)
				}
			}
			eprintln!("{}", AuditEvent::TrustRootAdopted { anchor });
			print_root(&repository).await?;
		}
		TrustSyncOutcome::Declined { .. } => {
			println!(
				"Declined the trust root from {}; {TRUST_REF} left unset.",
				origin.url
			);
		}
	}
	Ok(())
}

/// Decide whether to adopt an unseen trust root during a first-use `sync` bootstrap. With `--expect`,
/// adopt iff the chain's bootstrap `anchor` matches the pinned fingerprint (a mismatch is a hard
/// error). Otherwise, on a terminal, print the incoming root and prompt; off a terminal (gta-mcp, or
/// piped stdin) refuse — adopting an unverified root non-interactively is unsafe.
fn confirm_adoption(expect: &Option<String>, root: &TrustRoot, anchor: &KeyId) -> Result<bool> {
	if let Some(expected) = expect {
		let expected = expected.trim();
		if anchor.as_str() == expected {
			return Ok(true);
		}
		bail!(
			"the origin's trust root is anchored by {anchor}, not the expected {expected}; \
			 refusing to adopt it"
		);
	}
	if std::io::stdin().is_terminal() {
		return prompt_adoption(root, anchor);
	}
	bail!(
		"refusing to adopt an unverified trust root non-interactively; re-run with \
		 `--expect <fingerprint>` to pin the key that signed it (its anchor)"
	)
}

/// Print the incoming trust root and its anchor, then read a yes/no answer from the terminal.
fn prompt_adoption(root: &TrustRoot, anchor: &KeyId) -> Result<bool> {
	println!("The origin published a trust root this repository has not seen before:");
	println!("  policy: {}", root.policy);
	println!("  keys ({}):", root.keys.len());
	for key in &root.keys {
		println!("    {}", key.id());
	}
	println!("  anchored by (the key that signed the bootstrap): {anchor}");
	print!("Adopt this trust root? [y/N] ");
	std::io::stdout().flush().ok();
	let mut answer = String::new();
	std::io::stdin().read_line(&mut answer)?;
	let answer = answer.trim();
	Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

struct Trust {
	action: Action,
	/// The effective working directory, for resolving relative signing-key/public-key paths (`-C`).
	cwd: PathBuf,
}

impl RepoCommand for Trust {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let identity = CliIdentity::new(&repo);
		match self.action {
			Action::Init {
				policy,
				signing_key,
				break_glass,
				dry_run,
			} => {
				let policy = parse_policy(&policy)?;
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let pubkey = signer.public_line().await?;
				if dry_run {
					return init_preflight(&repo, policy, break_glass, &pubkey).await;
				}
				let (tip, event) =
					trust_init(&repo, policy, &pubkey, break_glass, &identity, &signer).await?;
				println!("Initialised trust root at {tip}");
				eprintln!("{event}");
				print_root(&repo).await
			}
			Action::List => print_root(&repo).await,
			Action::AddKey { key, signing_key } => {
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let key_line = read_key_arg(&key, &self.cwd).await?;
				let (tip, event) = trust_add_key(&repo, &key_line, &identity, &signer).await?;
				println!("Enrolled key; {TRUST_REF} now at {tip}");
				eprintln!("{event}");
				print_root(&repo).await
			}
			Action::RemoveKey {
				key,
				signing_key,
				break_glass,
			} => {
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let selector = read_key_arg(&key, &self.cwd).await?;
				let (tip, event) =
					trust_remove_key(&repo, &selector, break_glass, &identity, &signer).await?;
				println!("Removed key; {TRUST_REF} now at {tip}");
				eprintln!("{event}");
				print_root(&repo).await
			}
			Action::SetPolicy {
				policy,
				signing_key,
				break_glass,
				dry_run,
			} => {
				let policy = parse_policy(&policy)?;
				if dry_run {
					return set_policy_preflight(&repo, policy, break_glass).await;
				}
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let (tip, event) = trust_set_policy(&repo, policy, break_glass, &identity, &signer).await?;
				println!("Policy set to {policy}; {TRUST_REF} now at {tip}");
				eprintln!("{event}");
				print_root(&repo).await
			}
			// `sync` is handled in `run` before dispatch (it needs the origin + network), never here.
			Action::Sync { .. } => unreachable!("trust sync is dispatched before on_repo"),
		}
	}
}

/// Preflight for `trust init --dry-run`: report what bootstrapping would do, writing nothing.
async fn init_preflight<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	policy: Policy,
	break_glass: bool,
	pubkey: &str,
) -> Result<()> {
	println!("Dry run: `gta trust init` would bootstrap a trust root ({TRUST_REF}).");
	if repo.refs().resolve(TRUST_REF).await?.is_some() {
		println!(
			"  ! trust is already initialised; the real command would fail — use add-key/remove-key/set-policy."
		);
	}
	let id = TrustedKey::from_openssh(pubkey.trim())
		.context("parsing the signing public key")?
		.id();
	println!("  policy: {policy}");
	println!("  enrolling signing key: {id}");
	if policy == Policy::Require {
		print_single_key_warning(break_glass);
		print_require_implications();
	}
	println!("No changes made.");
	Ok(())
}

/// Preflight for `trust set-policy <policy> --dry-run`: report the cutover impact, writing nothing.
async fn set_policy_preflight<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	target: Policy,
	break_glass: bool,
) -> Result<()> {
	match trust_list(repo).await? {
		None => {
			println!("No trust root configured ({TRUST_REF} is unset); run `gta trust init` first.")
		}
		Some(root) => {
			println!(
				"Dry run: `gta trust set-policy` would change policy `{}` -> `{target}`.",
				root.policy
			);
			if root.policy == target {
				println!("  ! no change: policy is already `{target}` (the real command refuses a no-op).");
			}
			println!("  enrolled keys ({}):", root.keys.len());
			for key in &root.keys {
				println!("    {}", key.id());
			}
			if target == Policy::Require {
				// The `require` safety margin counts only push-capable (SSH) keys — OpenPGP certs are
				// verification-only and cannot sign a push — mirroring `trust_set_policy`.
				let ssh_keys = root
					.keys
					.iter()
					.filter(|key| matches!(key, TrustedKey::Ssh(_)))
					.count();
				if ssh_keys < 2 {
					print_single_key_warning(break_glass);
				}
				print_require_implications();
			}
		}
	}
	println!("No changes made.");
	Ok(())
}

/// The single-key `require` warning, worded for whether `--break-glass` was supplied — so the
/// preview matches what the real command would do.
fn print_single_key_warning(break_glass: bool) {
	if break_glass {
		println!(
			"  ! `require` with fewer than two SSH keys is unsafe (losing a key locks the repository; OpenPGP certs are verification-only); proceeding under `--break-glass`."
		);
	} else {
		println!(
			"  ! `require` with fewer than two SSH keys is unsafe (OpenPGP certs are verification-only); the real command needs `--break-glass`."
		);
	}
}

/// The enforcement `require` turns on: what future pushes must carry, and what is grandfathered.
fn print_require_implications() {
	println!(
		"Under `require`, pushes to protected refs (refs/heads/*, refs/tags/*, refs/gitana/*) will require:"
	);
	println!("  - a push certificate signed by a trusted key (`gta push --signed`), and");
	println!("  - a trusted signature on every newly introduced commit and annotated tag.");
	println!(
		"Existing history reachable from the current protected refs is grandfathered (no re-signing needed)."
	);
}

/// Print the current trust policy and enrolled key fingerprints, or a notice when trust is unset.
async fn print_root<H: HashAlgorithm>(repo: &Repository<Backend, H>) -> Result<()> {
	match trust_list(repo).await? {
		None => println!("No trust root configured ({TRUST_REF} is unset)."),
		Some(root) => print_trust_root(&root),
	}
	Ok(())
}

fn print_trust_root(root: &TrustRoot) {
	println!("policy: {}", root.policy);
	println!("keys ({}):", root.keys.len());
	for key in &root.keys {
		println!("  {}", key.id());
	}
}

/// Resolve a key argument to the value the porcelain expects: if it names a file, read the public key
/// out of it — an armored OpenPGP certificate verbatim, otherwise the OpenSSH public-key line;
/// if it does not name a file, pass it through verbatim (a literal OpenSSH/OpenPGP key, or — for
/// `remove-key` — a `SHA256:…`/OpenPGP fingerprint). A relative file path honors `-C` via `cwd`.
async fn read_key_arg(arg: &str, cwd: &Path) -> Result<String> {
	let path = cwd.join(arg);
	if path.is_file() {
		let contents = tokio::fs::read_to_string(&path)
			.await
			.with_context(|| format!("reading public key {}", path.display()))?;
		// An armored OpenPGP public key spans multiple lines and is used verbatim; an OpenSSH key is a
		// single line extracted from the file (e.g. a `.pub` or a private-key file's comment).
		if contents.contains("-----BEGIN PGP PUBLIC KEY BLOCK-----") {
			return Ok(contents.trim().to_owned());
		}
		return signer::public_key_line(&contents).ok_or_else(|| {
			anyhow!(
				"{} does not contain an OpenSSH or OpenPGP public key",
				path.display()
			)
		});
	}
	Ok(arg.trim().to_owned())
}

/// Parse the `--policy` value. Accepts git-trust's three policies; the default is `warn`.
fn parse_policy(value: &str) -> Result<Policy> {
	match value {
		"off" => Ok(Policy::Off),
		"warn" => Ok(Policy::Warn),
		"require" => Ok(Policy::Require),
		other => bail!("unknown policy `{other}` (expected off, warn, or require)"),
	}
}
