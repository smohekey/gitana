//! A read-only working-tree status readout, wrapping `gitana-worktree`'s three-way `status`, tied to
//! the inspected destination so a stale result is never applied to a replaced path. A status
//! *computation* that fails is a [`LinkedWorktreeError`] — never silently reported as clean.

use std::path::PathBuf;

use gitana_worktree::{Status, StatusEntry};

/// The status of one linked worktree's working tree, associated with its destination identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeStatusReport {
	/// The destination this status was computed for.
	pub destination: PathBuf,
	status: Status,
}

/// git's unmerged (conflict) index/worktree code pairs.
const CONFLICT_PAIRS: &[(char, char)] = &[
	('D', 'D'),
	('A', 'U'),
	('U', 'D'),
	('U', 'A'),
	('D', 'U'),
	('A', 'A'),
	('U', 'U'),
];

impl WorktreeStatusReport {
	/// The underlying three-way status (tracked changes with their `X`/`Y` codes, and untracked paths).
	pub fn status(&self) -> &Status {
		&self.status
	}

	/// Whether the working tree is clean — no tracked changes and no untracked paths.
	pub fn is_clean(&self) -> bool {
		self.status.changed.is_empty() && self.status.untracked.is_empty()
	}

	/// `git status --porcelain=v1` rendering.
	pub fn porcelain_v1(&self) -> String {
		self.status.porcelain_v1()
	}

	/// Whether any path has staged (index-vs-HEAD) changes.
	pub fn has_staged(&self) -> bool {
		self
			.status
			.changed
			.iter()
			.any(|e| !self.is_conflict(e) && e.index != ' ' && e.index != '?')
	}

	/// Whether any tracked path has unstaged (worktree-vs-index) modifications.
	pub fn has_unstaged(&self) -> bool {
		self
			.status
			.changed
			.iter()
			.any(|e| !self.is_conflict(e) && e.worktree == 'M')
	}

	/// Whether there are any untracked paths.
	pub fn has_untracked(&self) -> bool {
		!self.status.untracked.is_empty()
	}

	/// Whether any path is in an unmerged (conflicted) state.
	pub fn has_conflicts(&self) -> bool {
		self.status.changed.iter().any(|e| self.is_conflict(e))
	}

	/// Whether any tracked path is missing from the working tree (deleted, not staged as a deletion).
	pub fn has_missing(&self) -> bool {
		self
			.status
			.changed
			.iter()
			.any(|e| !self.is_conflict(e) && e.worktree == 'D')
	}

	fn is_conflict(&self, entry: &StatusEntry) -> bool {
		CONFLICT_PAIRS.contains(&(entry.index, entry.worktree))
	}
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use super::*;

	use std::path::Path;

	use gitana_object::{HashKind, Sha1, Sha256};
	use gitana_object_store::ObjectStore;
	use gitana_repository::Repository;
	use gitana_worktree::WorkTree;

	use crate::pointers::{
		admin_dirs_for, canonical_eq, checkout_gitfile_names, is_bare, is_leaf_symlink,
		main_checkout_identifies_common,
	};
	use crate::repo_id::{detect_kind, open_store_raw, open_work_dir};
	use crate::{LinkedWorktreeError, RepositoryId};

	/// Compute the working-tree status of the worktree at `destination` in repository `repo`. The
	/// destination must be a worktree of `repo` (its main worktree or a registered linked worktree);
	/// otherwise this is a hard error (a status cannot be attributed to a non-worktree path).
	pub async fn status(
		repo: &RepositoryId,
		destination: &Path,
	) -> Result<WorktreeStatusReport, LinkedWorktreeError> {
		if !destination.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(destination.to_path_buf()));
		}
		// A destination that is itself a symlink is never a worktree we status — following the alias to open
		// its target's `.git`/index would violate the no-follow boundary. It is a hard error, not a report.
		if is_leaf_symlink(destination) {
			return Err(LinkedWorktreeError::io(
				"status: destination is a symlink, not a worktree",
				destination,
				std::io::Error::from(std::io::ErrorKind::InvalidInput),
			));
		}
		let common = repo.common_dir();
		// A registered linked worktree — accepted only when its checkout is *live* (the checkout's `.git`
		// gitfile names the admin). A stale registration whose path was reused is not a worktree we can
		// status; it falls through to the hard error rather than opening an unrelated directory with the
		// stale admin index.
		// A single live registration is a linked worktree we can status. Zero, a duplicate (corruption), or
		// a stale registration (its checkout gone/reused) all fall through to the hard error below.
		let registered = match admin_dirs_for(common, destination)?.as_slice() {
			[admin] if checkout_gitfile_names(destination, admin)? => Some(admin.clone()),
			_ => None,
		};
		// The destination is the *main* worktree when its `.git` currently identifies `common` — an
		// ordinary main worktree's `.git` *is* `common` (a directory); a `--separate-git-dir` main
		// worktree's `.git` is a gitfile pointing at the external `common`. This identity check is the
		// authoritative test (it does not depend on how the `RepositoryId` was obtained — explicit
		// `at_common_dir`, discovery from the primary, or discovery from a linked worktree — and it closes
		// the replaced-checkout hole, since a moved/replaced separate-git-dir checkout no longer names
		// `common`). A bare repository has no main working tree, and the common dir itself is never one.
		let is_main = !is_bare(common)?
			&& !canonical_eq(destination, common)
			&& main_checkout_identifies_common(destination, common)?;
		// The per-worktree git dir holding this destination's index/HEAD.
		let git_dir = if let Some(admin) = registered {
			admin
		} else if is_main {
			common.to_path_buf()
		} else {
			return Err(LinkedWorktreeError::io(
				"status: not a worktree of this repository",
				destination,
				std::io::Error::from(std::io::ErrorKind::NotFound),
			));
		};

		let store = open_store_raw(&git_dir, common)?;
		let work = open_work_dir(destination)?;
		let status = match detect_kind(&store).await? {
			HashKind::Sha1 => {
				let repo = Repository::<_, Sha1>::new(ObjectStore::new(store));
				WorkTree::new(repo, work, git_dir).status().await?
			}
			HashKind::Sha256 => {
				let repo = Repository::<_, Sha256>::new(ObjectStore::new(store));
				WorkTree::new(repo, work, git_dir).status().await?
			}
		};
		Ok(WorktreeStatusReport {
			destination: destination.to_path_buf(),
			status,
		})
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::status;
