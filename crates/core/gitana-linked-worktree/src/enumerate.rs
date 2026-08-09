//! Structured enumeration of a repository's worktrees — the primary worktree first, then each linked
//! worktree (admin-name sorted, git's order). Reports the facts git's `worktree list --porcelain` does:
//! path, HEAD kind, branch, current object, missing-checkout, bare, and lock state.

use std::path::PathBuf;

use crate::WorktreeObjectId;
use crate::facts::{HeadKind, LockState};

/// A worktree's role in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeRole {
	/// The primary worktree (the main checkout, or the bare repository itself).
	Primary {
		/// Whether the repository is bare (no main checkout).
		bare: bool,
	},
	/// A linked worktree.
	Linked {
		/// Its admin directory `<common>/worktrees/<name>`.
		admin_dir: PathBuf,
	},
}

/// One enumerated worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
	/// Primary or linked (and, for primary, whether bare).
	pub role: WorktreeRole,
	/// The canonical checkout path.
	pub path: PathBuf,
	/// HEAD kind, or `None` for a bare repository (no checkout HEAD to report).
	pub head: Option<HeadKind>,
	/// The branch ref name when HEAD is symbolic; else `None`.
	pub branch: Option<String>,
	/// The current HEAD object; `None` when unborn or bare.
	pub object: Option<WorktreeObjectId>,
	/// The registration is present but the checkout path is gone (git's "prunable").
	pub checkout_missing: bool,
	/// The worktree's lock state.
	pub lock: LockState,
}

/// A repository's worktrees, primary first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeListing {
	/// The entries, primary first then linked (admin-name sorted).
	pub entries: Vec<WorktreeEntry>,
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use super::*;

	use std::path::Path;

	use gitana_config::GitConfig;
	use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
	use gitana_object_store::ObjectStore;
	use gitana_repository::{HeadState, Repository};

	use crate::head::{read_head, read_lock_reason};
	use crate::object_id::IntoWorktreeObjectId;
	use crate::pointers::{
		RefSource, SYMREF_MAXDEPTH, admin_checkout_missing, canonical, ignorecase, is_bare,
		linked_admin_dirs, main_worktree_path, resolve_ref_terminal, worktree_path_of,
	};
	use crate::repo_id::{
		detect_kind, open_store_raw, reject_unsupported_repository_format,
		validate_repository_structure,
	};
	use crate::{LinkedWorktreeError, WorktreeContext};

	/// Enumerate the worktrees of the repository `cx` names.
	///
	/// The listing order follows `core.ignorecase` as resolved by `cx` — the caller's merged config stack
	/// when injected, else the repository-local config alone.
	pub async fn enumerate(cx: &WorktreeContext) -> Result<WorktreeListing, LinkedWorktreeError> {
		let common = cx.repo().common_dir();
		reject_unsupported_repository_format(common)?;
		validate_repository_structure(common)?;
		// The initial store only detects the object format (shared state) — anchor it on the stable
		// `common_dir`, not the identity's `git_dir` (which, discovered inside a linked worktree, names that
		// checkout's admin and fails to open once the checkout is pruned). Each worktree's HEAD is then
		// resolved through a store scoped to *that* worktree's git dir (see `head_facts`).
		let store = open_store_raw(common, common)?;
		let effective = cx.effective_config();
		match detect_kind(&store).await? {
			HashKind::Sha1 => enumerate_generic::<Sha1>(common, effective).await,
			HashKind::Sha256 => enumerate_generic::<Sha256>(common, effective).await,
		}
	}

	async fn enumerate_generic<H: HashAlgorithm>(
		common: &Path,
		effective: Option<&GitConfig>,
	) -> Result<WorktreeListing, LinkedWorktreeError>
	where
		ObjectId<H>: IntoWorktreeObjectId,
	{
		let mut entries = Vec::new();

		// The primary worktree first.
		if is_bare(common)? {
			entries.push(WorktreeEntry {
				role: WorktreeRole::Primary { bare: true },
				path: canonical(common),
				head: None,
				branch: None,
				object: None,
				checkout_missing: false,
				lock: LockState::Unlocked,
			});
		} else {
			// The primary git dir is not a linked admin: derive its path directly from `common` (never from a
			// `gitdir` file) and keep its lock `Unlocked` — git ignores a stray `gitdir`/`locked` in the main
			// `.git`. git never marks the main worktree prunable, so `checkout_missing` is always false.
			let path = canonical(&main_worktree_path(common));
			let (head, branch, object) = head_facts::<H>(common, common).await?;
			entries.push(WorktreeEntry {
				role: WorktreeRole::Primary { bare: false },
				checkout_missing: false,
				path,
				head,
				branch,
				object,
				lock: LockState::Unlocked,
			});
		}

		// Then each linked worktree under `<common>/worktrees/*` (a scan failure is an error).
		// `checkout_missing` is git's own prunable test (`admin_checkout_missing`): the `<admin>/gitdir`
		// pointer target no longer exists. git lists (never prunes) a checkout whose `.git` is merely
		// foreign/broken, so this must *not* use the stricter identity check inspection/removal rely on.
		let mut linked = Vec::new();
		for admin in linked_admin_dirs(common)? {
			let path = canonical(&worktree_path_of(&admin)?);
			let (head, branch, object) = head_facts::<H>(common, &admin).await?;
			linked.push(WorktreeEntry {
				role: WorktreeRole::Linked {
					admin_dir: admin.clone(),
				},
				checkout_missing: admin_checkout_missing(&admin)?,
				path,
				head,
				branch,
				object,
				lock: read_lock_reason(&admin),
			});
		}
		// git's `worktree list` orders linked worktrees by *checkout path*, not admin name — and compares
		// case-insensitively when `core.ignorecase` is set (typical on macOS/Windows). Match both.
		if ignorecase(effective, common) {
			linked.sort_by(|a, b| {
				a.path
					.to_string_lossy()
					.to_ascii_lowercase()
					.cmp(&b.path.to_string_lossy().to_ascii_lowercase())
			});
		} else {
			linked.sort_by(|a, b| a.path.cmp(&b.path));
		}
		entries.extend(linked);

		Ok(WorktreeListing { entries })
	}

	/// Read a git dir's HEAD and resolve its object through a store scoped to **that** worktree's `git_dir`
	/// (with `common` as the shared dir), so a per-worktree ref target is read from the right namespace.
	async fn head_facts<H: HashAlgorithm>(
		common: &Path,
		git_dir: &Path,
	) -> Result<(Option<HeadKind>, Option<String>, Option<WorktreeObjectId>), LinkedWorktreeError>
	where
		ObjectId<H>: IntoWorktreeObjectId,
	{
		match read_head::<H>(git_dir)? {
			None => Ok((None, None, None)),
			Some(HeadState::Symbolic(refname)) => {
				// Report the *terminal* branch (git's worktree list shows `feature` for HEAD → alias →
				// feature). Resolve the object through this worktree's own store; `resolve_symbolic` follows
				// the same chain.
				// `refname` is `HEAD`'s target (`HEAD` already read), so one hop of git's budget is spent.
				let terminal = resolve_ref_terminal(
					common,
					git_dir,
					&refname,
					RefSource::Head,
					SYMREF_MAXDEPTH - 1,
				)?;
				let repo = Repository::<_, H>::new(ObjectStore::new(open_store_raw(git_dir, common)?));
				// Resolve through the *terminal* ref: a legacy *symlink* symref is symbolic to git, but the
				// filesystem backend following the link relative to `refs/heads` would miss the object.
				let object = repo
					.refs()
					.resolve_symbolic(&terminal)
					.await?
					.map(IntoWorktreeObjectId::tag);
				let kind = if object.is_some() {
					HeadKind::Symbolic
				} else {
					HeadKind::Unborn
				};
				Ok((Some(kind), Some(terminal), object))
			}
			Some(HeadState::Detached(id)) => Ok((Some(HeadKind::Detached), None, Some(id.tag()))),
		}
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::enumerate;
