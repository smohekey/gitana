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

		// Hold this worktree's `HEAD.lock` for the whole checkout — from before the merge base is read
		// through publishing the new `HEAD`. This serializes the checkout against a concurrent `HEAD`
		// retarget and against a ref transaction moving the branch this worktree currently has checked
		// out (such a move locks `HEAD` for its reflog cascade), so the two-tree merge base, the working-
		// tree mutation, and the final `HEAD` publish cannot interleave with them. The lock releases on
		// drop if the checkout bails or is cancelled before that publish, leaving `HEAD` unmoved.
		let head_lock = repo.refs().lock_head().await?;

		// Describe what HEAD points at now, for the `from` half of the checkout reflog — read *after*
		// acquiring the lock so overlapping switches serialize and each records its real starting HEAD,
		// not one a concurrent switch has since moved.
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
		// A non-force switch is git's two-tree merge (`read-tree -m -u`) from the current HEAD's tree to the
		// target: it carries local staged/unstaged work across the switch, refusing only real conflicts —
		// rather than treating the target as authoritative and silently dropping staged changes. `--force`
		// keeps the authoritative reset, and does NOT resolve the current HEAD's tree — a `switch --force`
		// must be able to recover even when the *current* branch's tip is missing/corrupt.
		// git refuses to move HEAD while a merge/cherry-pick/revert/rebase is unconcluded, so the operation
		// state is not left attached to another branch (a later `commit` could finish it on the wrong one).
		// A resolved-but-not-committed merge (`MERGE_HEAD` present, no conflict stages) still counts, which
		// the two-tree merge's unmerged-index check alone would miss. This runs even under `--force`: git
		// refuses `switch -f`/`checkout -f` too while merging/rebasing (probed vs git 2.55: "cannot switch
		// branch while merging/rebasing") — force overrides working-tree overwrite protection, not the
		// operation-state guard.
		if let Some(op) = gitana_porcelain::conflict::operation_in_progress(repo).await? {
			// The hint names `gta {op} --abort`, but the operation may have been started by stock git (a
			// `rebase-merge/` / `rebase-apply/` rebase), whose state gta's own `--abort` does not read — so
			// point at the tool that started it too, rather than promising a command that would report "no
			// {op} in progress".
			bail!(
				"cannot switch branch while {op} is in progress\nconclude or abort it first (\"gta {op} --abort\", or the tool that started it)"
			);
		}
		if self.force {
			worktree
				.checkout(tree, true, excludes_file.as_deref())
				.await?;
		} else {
			// The tree the index currently matches: the current HEAD's tree, or the empty tree for an unborn
			// HEAD — so staged work created on an orphan branch is still carried across (git does).
			let head_tree = match repo.refs().resolve_head().await? {
				Some(commit) => repo.commit_tree(commit).await?,
				None => repo.write_tree(&[]).await?,
			};
			worktree
				.checkout_merge(head_tree, tree, excludes_file.as_deref())
				.await?;
		}
		// git records the start point as named, defaulting to the literal `HEAD` for `switch -c` (unlike
		// `branch`, which defaults to the current branch's name). The create asserts the branch's absence
		// (`expected: None`), so a name created concurrently since the early existence check above fails
		// here rather than being overwritten.
		let created_from = self.start.as_deref().unwrap_or("HEAD");
		let create_message = format!("branch: Created from {created_from}");
		let create = self.create.then(|| {
			(
				target,
				ReflogIntent::Log {
					committer: &committer,
					message: &create_message,
				},
			)
		});
		// Publish the checkout by consuming the `HEAD.lock` held across the whole checkout: create the
		// branch (if `-c`) and set `HEAD` in one owned task that keeps the lock — never a fresh
		// `set_head_symbolic`/`update_ref`, which would try to re-acquire the lock we already own. When
		// `HEAD` is on the (unborn) branch being created, the create cascades into `logs/HEAD` and is
		// written under the held lock.
		let checkout_message = format!("checkout: moving from {from} to {}", self.name);
		head_lock
			.finish_checkout(
				&branch,
				create,
				ReflogIntent::Log {
					committer: &committer,
					message: &checkout_message,
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
