use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_repository::{ReflogIntent, Repository};
use gitana_worktree::WorkTree;

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::signature_or_default;

/// Switch the working tree and `HEAD` to branch `name`. With `create`, make the
/// branch (at `start`, default `HEAD`) first. With `force`, overwrite local changes.
pub async fn run(
	cwd: &Path,
	name: &str,
	create: bool,
	start: Option<String>,
	force: bool,
) -> Result<()> {
	dispatch::on_worktree(
		cwd,
		Switch {
			cwd: cwd.to_owned(),
			name,
			create,
			start,
			force,
		},
	)
	.await
}

struct Switch<'a> {
	cwd: std::path::PathBuf,
	name: &'a str,
	create: bool,
	start: Option<String>,
	force: bool,
}

impl WorkTreeCommand for Switch<'_> {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: String,
	) -> Result<()> {
		let repo = worktree.repository();
		let branch = format!("refs/heads/{}", self.name);
		let committer = signature_or_default(repo, "COMMITTER").await?;
		// Describe what HEAD points at now, before it moves — the `from` half of the checkout reflog.
		let from = head_description(repo).await?;

		// Resolve the commit to check out. The branch-already-exists check stays here, before any tree
		// work, as git does it; the branch itself is created only *after* a successful checkout (below), so a
		// failed checkout leaves no ref behind — no rollback needed.
		let target = if self.create {
			if repo.refs().resolve(&branch).await?.is_some() {
				bail!("a branch named '{}' already exists", self.name);
			}
			repo
				.rev_parse(self.start.as_deref().unwrap_or("HEAD"))
				.await?
		} else {
			match repo.refs().resolve(&branch).await? {
				Some(commit) => commit,
				None => bail!("invalid reference: {}", self.name),
			}
		};

		// A branch's ref is shared across a repository's worktrees, so git forbids checking the same
		// branch out in two of them at once (their commits would race on one ref). Refuse before
		// touching the working tree, as git does.
		if let Some(other) =
			crate::repo::branch_checked_out_elsewhere(worktree.git_dir(), &branch).await?
		{
			bail!(
				"'{}' is already checked out at '{}'",
				self.name,
				other.display()
			);
		}

		let tree = repo.commit_tree(target).await?;
		// git's global excludes file (`core.excludesFile`) content. Resolved for every switch — git
		// validates it (a directory is fatal) even under `--force`, which skips the overwrite *protection*,
		// not config validation.
		let config = repo.effective_config().await?;
		let excludes_file = crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?;

		// Check out the working tree BEFORE publishing the branch — git's order (probed vs git 2.55: a
		// `switch -c` whose branch ref cannot be locked still updates the working tree, then fails to create
		// the branch and leaves HEAD unmoved). Creating the branch only after a successful checkout keeps
		// `switch -c` failure-atomic with no leftover ref AND needs no rollback: an earlier reserve-then-
		// rollback design could race a concurrent worktree adopting the just-created branch and delete it out
		// from under that worktree's HEAD (a dangling HEAD). A failed checkout now simply creates nothing.
		worktree
			.checkout(tree, self.force, excludes_file.as_deref())
			.await?;
		if self.create {
			// git records the start point as named, defaulting to the literal `HEAD` for `switch -c`
			// (unlike `branch`, which defaults to the current branch's name). `update_ref` with `expected:
			// None` also re-asserts the branch's absence, so a name created concurrently since the early
			// existence check above fails here rather than being overwritten.
			let created_from = self.start.as_deref().unwrap_or("HEAD");
			let message = format!("branch: Created from {created_from}");
			repo
				.refs()
				.update_ref(
					&branch,
					target,
					None,
					ReflogIntent::Log {
						committer: &committer,
						message: &message,
					},
				)
				.await?;
		}
		let message = format!("checkout: moving from {from} to {}", self.name);
		repo
			.refs()
			.set_head_symbolic(
				&branch,
				ReflogIntent::Log {
					committer: &committer,
					message: &message,
				},
			)
			.await?;
		eprintln!("Switched to branch '{}'", self.name);
		Ok(())
	}
}

/// Describe `HEAD` for a checkout reflog: the current branch's short name, or `HEAD`'s full object
/// id when it is detached.
async fn head_description<H: HashAlgorithm>(repo: &Repository<Backend, H>) -> Result<String> {
	match repo.refs().read_symbolic("HEAD").await? {
		Some(target) => Ok(
			target
				.strip_prefix("refs/heads/")
				.unwrap_or(&target)
				.to_owned(),
		),
		None => Ok(repo.rev_parse("HEAD").await?.to_hex()),
	}
}
