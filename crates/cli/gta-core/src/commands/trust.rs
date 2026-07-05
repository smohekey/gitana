//! `gta trust` — manage the repository's trust root (the signed `refs/gitana/trust` chain). `init`
//! bootstraps a self-signed root; `add-key`/`remove-key` and `set-policy` extend the chain with new
//! signed updates; `list` shows the current policy and enrolled key fingerprints.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use gitana_object::HashAlgorithm;
use gitana_porcelain::{
	TRUST_REF, trust_add_key, trust_init, trust_list, trust_remove_key, trust_set_policy,
};
use gitana_repository::Repository;
use gitana_trust::{Policy, TrustRoot};

use crate::Backend;
use crate::dispatch::{self, RepoCommand};
use crate::identity::CliIdentity;
use crate::signer::{self, CliSigner};

/// A `trust` sub-command.
pub enum Action {
	/// Bootstrap the trust root, enrolling the signing key under `policy`.
	Init {
		policy: String,
		signing_key: Option<PathBuf>,
		break_glass: bool,
	},
	/// Show the current policy and enrolled key fingerprints.
	List,
	/// Enrol a public key (a `.pub` file path or a literal OpenSSH line).
	AddKey {
		key: String,
		signing_key: Option<PathBuf>,
	},
	/// Remove a key by fingerprint (`SHA256:…`) or public-key line/file.
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
	},
}

/// Run a `trust` sub-command in the repository containing `cwd`.
pub async fn run(cwd: &Path, action: Action) -> Result<()> {
	dispatch::on_repo(
		cwd,
		Trust {
			action,
			cwd: cwd.to_path_buf(),
		},
	)
	.await
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
			} => {
				let policy = parse_policy(&policy)?;
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let pubkey = signer.public_line().await?;
				let tip = trust_init(&repo, policy, &pubkey, break_glass, &identity, &signer).await?;
				println!("Initialised trust root at {tip}");
				print_root(&repo).await
			}
			Action::List => print_root(&repo).await,
			Action::AddKey { key, signing_key } => {
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let key_line = read_key_arg(&key, &self.cwd).await?;
				let tip = trust_add_key(&repo, &key_line, &identity, &signer).await?;
				println!("Enrolled key; {TRUST_REF} now at {tip}");
				print_root(&repo).await
			}
			Action::RemoveKey {
				key,
				signing_key,
				break_glass,
			} => {
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let selector = read_key_arg(&key, &self.cwd).await?;
				let tip = trust_remove_key(&repo, &selector, break_glass, &identity, &signer).await?;
				println!("Removed key; {TRUST_REF} now at {tip}");
				print_root(&repo).await
			}
			Action::SetPolicy {
				policy,
				signing_key,
				break_glass,
			} => {
				let policy = parse_policy(&policy)?;
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let tip = trust_set_policy(&repo, policy, break_glass, &identity, &signer).await?;
				println!("Policy set to {policy}; {TRUST_REF} now at {tip}");
				print_root(&repo).await
			}
		}
	}
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

/// Resolve a key argument to the value the porcelain expects: if it names a file, read the OpenSSH
/// public-key line out of it; otherwise pass it through verbatim (a literal key line, or — for
/// `remove-key` — a `SHA256:…` fingerprint). A relative file path honors `-C` via `cwd`.
async fn read_key_arg(arg: &str, cwd: &Path) -> Result<String> {
	let path = cwd.join(arg);
	if path.is_file() {
		let contents = tokio::fs::read_to_string(&path)
			.await
			.with_context(|| format!("reading public key {}", path.display()))?;
		return signer::public_key_line(&contents)
			.ok_or_else(|| anyhow!("{} does not contain an OpenSSH public key", path.display()));
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
