//! The CLI side of SSH signing: resolve a signing key and shell out to `ssh-keygen -Y sign` — the
//! same mechanism stock git drives through `gpg.ssh.program`, so the signatures interoperate with
//! git and the key can be in any format `ssh-keygen` reads. The subprocess is awaited through
//! `tokio::process` so it never blocks the runtime (see `docs/conventions.md`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use gitana_object::HashAlgorithm;
use gitana_porcelain::Signer;
use gitana_repository::Repository;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::Backend;

/// A [`Signer`] backed by a local SSH private key, signing via `ssh-keygen -Y sign` in git's `git`
/// namespace.
pub(crate) struct CliSigner {
	key_path: PathBuf,
}

impl CliSigner {
	/// Resolve the signing key from `--signing-key <path>`, or git config `user.signingkey` when the
	/// flag is absent, and confirm the file exists. Errors when neither is set — signing needs a key.
	///
	/// A leading `~`/`~/` is expanded against `$HOME` (as git and ssh do — SSH signing keys commonly
	/// live at `~/.ssh/...`). An otherwise-relative key path (from either source) resolves against
	/// `cwd`, the effective working directory — so `gta -C <dir>` behaves as if started in `<dir>`,
	/// matching git.
	pub(crate) async fn resolve<H: HashAlgorithm>(
		repo: &Repository<Backend, H>,
		signing_key: Option<PathBuf>,
		cwd: &Path,
	) -> Result<Self> {
		let configured = match signing_key {
			Some(path) => path,
			None => {
				let config = repo.read_config().await.ok();
				let configured = config
					.as_ref()
					.and_then(|config| config.get_string("user", None, "signingkey"))
					.ok_or_else(|| {
						anyhow!("no signing key: pass --signing-key <path> or set git config `user.signingkey`")
					})?;
				PathBuf::from(configured)
			}
		};
		// Expand `~` first (yielding an absolute path), then `join`: `join` leaves an absolute path
		// unchanged and resolves a relative one against the effective cwd.
		let key_path = cwd.join(expand_tilde(configured)?);
		if !key_path.exists() {
			bail!("signing key not found: {}", key_path.display());
		}
		Ok(Self { key_path })
	}

	/// The OpenSSH public-key line for this signing key — what a trust document enrols so the key can
	/// be trusted to verify what it signs.
	///
	/// `user.signingkey` commonly points straight at a public key file (`~/.ssh/id_ed25519.pub`); when
	/// it does, that line *is* the answer, so read it directly. Otherwise the path is a private key and
	/// we derive its public half with `ssh-keygen -y` (which would fail on a public key).
	pub(crate) async fn public_line(&self) -> Result<String> {
		let contents = tokio::fs::read_to_string(&self.key_path)
			.await
			.with_context(|| format!("reading signing key {}", self.key_path.display()))?;
		if let Some(line) = public_key_line(&contents) {
			return Ok(line);
		}

		let output = Command::new("ssh-keygen")
			.arg("-y")
			.arg("-f")
			.arg(&self.key_path)
			.output()
			.await
			.context("running `ssh-keygen -y` (is ssh-keygen installed?)")?;
		if !output.status.success() {
			bail!(
				"`ssh-keygen -y` failed: {}",
				String::from_utf8_lossy(&output.stderr).trim()
			);
		}
		Ok(
			String::from_utf8(output.stdout)
				.context("ssh-keygen public-key output was not UTF-8")?
				.trim_end()
				.to_owned(),
		)
	}
}

impl Signer for CliSigner {
	async fn sign(&self, payload: &[u8]) -> Result<String> {
		let mut child = Command::new("ssh-keygen")
			.arg("-Y")
			.arg("sign")
			.arg("-n")
			.arg("git")
			.arg("-f")
			.arg(&self.key_path)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.context("spawning `ssh-keygen -Y sign` (is ssh-keygen installed?)")?;

		// Feed the payload on stdin; closing it (drop) signals EOF so ssh-keygen finishes.
		let mut stdin = child.stdin.take().expect("stdin was piped");
		stdin
			.write_all(payload)
			.await
			.context("writing payload to ssh-keygen")?;
		drop(stdin);

		let output = child
			.wait_with_output()
			.await
			.context("waiting for ssh-keygen")?;
		if !output.status.success() {
			bail!(
				"`ssh-keygen -Y sign` failed: {}",
				String::from_utf8_lossy(&output.stderr).trim()
			);
		}
		// Trim the trailing newline ssh-keygen prints: the `Signer` contract is a bare armor block,
		// which the commit encoder folds into the `gpgsig` header.
		Ok(
			String::from_utf8(output.stdout)
				.context("ssh-keygen signature output was not UTF-8")?
				.trim_end()
				.to_owned(),
		)
	}
}

/// A lazily-resolved [`CliSigner`]: it validates `gpg.format`, loads the signing key, and runs
/// `ssh-keygen` only on the first `sign` call. So an operation that records **no** commit — a no-op
/// `gta commit`, a fast-forward `merge`/`pull`, an up-to-date `rebase` — never touches signing config
/// at all (it must not fail on an unsupported `gpg.format` or a missing key when nothing is signed),
/// while one that records several (a rebase replay) resolves once and reuses. Every history operation
/// is handed this behind an `Option<&LazyCliSigner>`.
pub(crate) struct LazyCliSigner<'a, H: HashAlgorithm> {
	repo: &'a Repository<Backend, H>,
	signing_key: Option<PathBuf>,
	cwd: PathBuf,
	/// Whether an unset `gpg.format` is rejected (config-driven signing) or assumed `ssh` (explicit
	/// `-S`). Either way an explicit non-`ssh` format is always rejected — gitana signs only with SSH.
	require_explicit_ssh: bool,
	resolved: tokio::sync::OnceCell<CliSigner>,
}

impl<'a, H: HashAlgorithm> LazyCliSigner<'a, H> {
	/// A signer that will resolve `signing_key` (or git config `user.signingkey`) against `cwd`, and
	/// enforce `gpg.format`, when first asked to sign. `require_explicit_ssh` distinguishes the
	/// config-driven policy (unset `gpg.format` is an error — git's default is OpenPGP) from the
	/// explicit `-S` policy (unset is assumed `ssh`).
	pub(crate) fn new(
		repo: &'a Repository<Backend, H>,
		signing_key: Option<PathBuf>,
		cwd: PathBuf,
		require_explicit_ssh: bool,
	) -> Self {
		Self {
			repo,
			signing_key,
			cwd,
			require_explicit_ssh,
			resolved: tokio::sync::OnceCell::new(),
		}
	}

	/// Validate `gpg.format` and resolve the underlying [`CliSigner`]. Called once, the first time a
	/// commit is actually signed — deferring both checks off the no-commit paths.
	async fn resolve(&self) -> Result<CliSigner> {
		let config = self.repo.read_config().await?;
		match config.get_string("gpg", None, "format") {
			Some("ssh") => {}
			Some(other) => bail!(
				"cannot sign: gitana produces only SSH signatures, but git config `gpg.format` is \
				 `{other}`; set `gpg.format ssh` to sign with gitana"
			),
			None if self.require_explicit_ssh => bail!(
				"cannot sign: git config `commit.gpgsign` is set but `gpg.format` is unset (git's default \
				 is OpenPGP); gitana signs only with SSH — set `gpg.format ssh`"
			),
			None => {} // explicit `-S`: assume ssh, the only format gitana signs
		}
		CliSigner::resolve(self.repo, self.signing_key.clone(), &self.cwd).await
	}
}

impl<H: HashAlgorithm> Signer for LazyCliSigner<'_, H> {
	async fn sign(&self, payload: &[u8]) -> Result<String> {
		let signer = self.resolved.get_or_try_init(|| self.resolve()).await?;
		signer.sign(payload).await
	}
}

/// The signer for git config-driven signing on the history operations (merge/cherry-pick/revert/
/// rebase/pull, and `gta commit` with no explicit flag): a [`LazyCliSigner`] over `user.signingkey`
/// when [`config_requests_signing`] is true, else `None`. The op passes it as `Option<&LazyCliSigner>`.
/// `gpg.format` is validated lazily by the signer, so a signing-configured repo can still fast-forward.
pub(crate) async fn config_signer<'a, H: HashAlgorithm>(
	repo: &'a Repository<Backend, H>,
	cwd: &Path,
) -> Result<Option<LazyCliSigner<'a, H>>> {
	Ok(
		config_requests_signing(repo)
			.await?
			.then(|| LazyCliSigner::new(repo, None, cwd.to_path_buf(), true)),
	)
}

/// Whether git config `commit.gpgSign` requests commit signing (see [`config_requests_gpgsign`]).
pub(crate) async fn config_requests_signing<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
) -> Result<bool> {
	config_requests_gpgsign(repo, "commit").await
}

/// Whether git config `tag.gpgSign` requests signing of annotated tags (`gta tag -a` with no explicit
/// `-s`). The tag analog of [`config_requests_signing`], and equally fails *closed*.
pub(crate) async fn config_requests_tag_signing<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
) -> Result<bool> {
	config_requests_gpgsign(repo, "tag").await
}

/// Read `<section>.gpgSign` (git's boolean signing switch), defaulting to `false`. A config read/parse
/// error propagates rather than silently dropping to unsigned. `gpg.format` is deferred to
/// [`LazyCliSigner`], so an operation that records nothing never fails on it.
async fn config_requests_gpgsign<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	section: &str,
) -> Result<bool> {
	let config = repo.read_config().await?;
	Ok(config.get_bool(section, None, "gpgsign")?.unwrap_or(false))
}

/// Expand a leading `~` (`~` or `~/…`) against `$HOME`, as git and ssh do for `user.signingkey` —
/// keys commonly live at `~/.ssh/…`. Any other path (including `~user/…`, which we do not resolve) is
/// returned unchanged. Errors only when a `~` needs expanding but `$HOME` is unset.
fn expand_tilde(path: PathBuf) -> Result<PathBuf> {
	let Some(rest) = path.to_str().and_then(|path| path.strip_prefix('~')) else {
		return Ok(path);
	};
	// `~` alone or `~/…`; `~user/…` (a non-empty, non-`/` remainder) is left for the caller to fail on.
	if !rest.is_empty() && !rest.starts_with('/') {
		return Ok(path);
	}
	let home = std::env::var_os("HOME")
		.ok_or_else(|| anyhow!("cannot expand `~` in signing key path: $HOME is not set"))?;
	// Strip the leading `/` off `rest` so it joins as a relative segment onto $HOME.
	Ok(PathBuf::from(home).join(rest.strip_prefix('/').unwrap_or(rest)))
}

/// If `contents` is an OpenSSH public key (its first non-blank line begins with a public key-type
/// token, as in `authorized_keys`), return that line. A private key file (`-----BEGIN … PRIVATE
/// KEY-----`) yields `None`.
pub(crate) fn public_key_line(contents: &str) -> Option<String> {
	const KEY_TYPE_PREFIXES: [&str; 3] = ["ssh-", "ecdsa-", "sk-"];
	let first = contents
		.lines()
		.find(|line| !line.trim().is_empty())?
		.trim();
	KEY_TYPE_PREFIXES
		.iter()
		.any(|prefix| first.starts_with(prefix))
		.then(|| first.to_owned())
}

#[cfg(test)]
mod tests {
	use super::*;

	const PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample admin@example.com";

	#[test]
	fn recognises_a_public_key_line() {
		assert_eq!(public_key_line(&format!("{PUB}\n")).as_deref(), Some(PUB));
		assert_eq!(
			public_key_line("ecdsa-sha2-nistp256 AAAA... e@x").as_deref(),
			Some("ecdsa-sha2-nistp256 AAAA... e@x")
		);
	}

	#[test]
	fn rejects_a_private_key_file() {
		let private =
			"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1r...\n-----END OPENSSH PRIVATE KEY-----\n";
		assert_eq!(public_key_line(private), None);
	}

	#[test]
	fn expands_a_leading_tilde_against_home() {
		let home = PathBuf::from(std::env::var_os("HOME").expect("HOME set in the test environment"));
		// `~/…` expands under $HOME; a bare `~` is $HOME itself.
		assert_eq!(
			expand_tilde(PathBuf::from("~/.ssh/id_ed25519")).unwrap(),
			home.join(".ssh/id_ed25519")
		);
		assert_eq!(expand_tilde(PathBuf::from("~")).unwrap(), home);
	}

	#[test]
	fn leaves_other_paths_unchanged() {
		// Absolute, ordinary-relative, and unsupported `~user/…` paths pass through verbatim.
		for path in ["/abs/key", "rel/key", "~user/key"] {
			assert_eq!(
				expand_tilde(PathBuf::from(path)).unwrap(),
				PathBuf::from(path)
			);
		}
	}
}
