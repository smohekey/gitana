use std::path::{Path, PathBuf};

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;
use crate::signer::{self, LazyCliSigner};

/// Create a commit from the index on the current branch. `sign`/`no_sign` are the explicit
/// `-S`/`--no-gpg-sign` flags; absent, signing follows git config `commit.gpgsign`. `signing_key`
/// overrides `user.signingkey`.
pub async fn run(
	cwd: &Path,
	message: &str,
	sign: bool,
	no_sign: bool,
	signing_key: Option<PathBuf>,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Commit {
			message,
			sign,
			no_sign,
			signing_key,
			cwd: cwd.to_path_buf(),
		},
	)
	.await
}

struct Commit<'a> {
	message: &'a str,
	sign: bool,
	no_sign: bool,
	signing_key: Option<PathBuf>,
	/// The effective working directory, for resolving a relative signing-key path (`-C`).
	cwd: PathBuf,
}

impl WorkTreeCommand for Commit<'_> {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		_prefix: String,
	) -> Result<()> {
		let repo = worktree.repository();
		let identity = CliIdentity::new(repo);
		// Whether this commit should be signed (`-S` / `--no-gpg-sign` / `commit.gpgsign`). The signer
		// picks its format from `gpg.format` and resolves its key lazily, only once a commit is certain —
		// so a no-op reports "nothing to commit" first, never a signing-config error.
		let signer = self
			.should_sign(repo)
			.await?
			.then(|| LazyCliSigner::new(repo, self.signing_key, self.cwd));
		let signer = signer.as_ref();

		// A rebase replays commits itself; a plain `gta commit` would create a stray commit the
		// sequencer doesn't track, so direct the user to `gta rebase --continue`.
		if repo.rebase_in_progress().await? {
			bail!(
				"you are in the middle of a rebase; run `gta rebase --continue` instead of `gta commit`"
			);
		}
		// Concluding an in-progress operation: each porcelain `continue_*` produces the right shape of
		// commit (two-parent merge / author-preserving pick / reverter-authored revert), signs it when
		// `signer` is set, and clears its state.
		if repo.merge_head().await?.is_some() {
			let commit = gitana_porcelain::continue_merge(
				&worktree,
				Some(self.message.to_owned()),
				&identity,
				signer,
			)
			.await?;
			println!("{commit}");
			return Ok(());
		}
		if repo.cherry_pick_head().await?.is_some() {
			let commit = gitana_porcelain::continue_cherry_pick(
				&worktree,
				Some(self.message.to_owned()),
				&identity,
				signer,
			)
			.await?;
			println!("{commit}");
			return Ok(());
		}
		if repo.revert_head().await?.is_some() {
			let commit = gitana_porcelain::continue_revert(
				&worktree,
				Some(self.message.to_owned()),
				&identity,
				signer,
			)
			.await?;
			println!("{commit}");
			return Ok(());
		}

		// Plain commit: the porcelain operation records the staged tree (refusing an unmerged or empty
		// index first), resolving the git identity — and, when signing, the signing key — only if a
		// commit will actually be made.
		let id = match signer {
			Some(signer) => {
				gitana_porcelain::commit_signed(&worktree, self.message, &identity, signer).await?
			}
			None => gitana_porcelain::commit(&worktree, self.message, &identity).await?,
		};
		println!("{id}");
		Ok(())
	}
}

impl Commit<'_> {
	/// Whether this commit should be signed: `--no-gpg-sign` wins (off), then `-S`/`--gpg-sign`, then
	/// git config `commit.gpgsign`. Fails *closed*: a config read/parse error propagates rather than
	/// silently dropping to unsigned. The signing format (`gpg.format`) and key are resolved by the
	/// signer, lazily — only once a commit is certain.
	async fn should_sign<H: HashAlgorithm>(&self, repo: &Repository<Backend, H>) -> Result<bool> {
		if self.no_sign {
			return Ok(false);
		}
		if self.sign {
			return Ok(true);
		}
		signer::config_requests_signing(repo).await
	}
}
