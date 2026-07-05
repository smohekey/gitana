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
	/// A relative key path (from either source) resolves against `cwd`, the effective working
	/// directory — so `gta -C <dir>` behaves as if started in `<dir>`, matching git.
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
		// `join` leaves an absolute path unchanged and resolves a relative one against the effective cwd.
		let key_path = cwd.join(configured);
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

/// If `contents` is an OpenSSH public key (its first non-blank line begins with a public key-type
/// token, as in `authorized_keys`), return that line. A private key file (`-----BEGIN … PRIVATE
/// KEY-----`) yields `None`.
fn public_key_line(contents: &str) -> Option<String> {
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
}
