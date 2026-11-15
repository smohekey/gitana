//! The working tree and git index (staging area).
//!
//! Owns a git-compatible `.git/index` (DIRC, read v2–v4 / write v4, sha256) and,
//! in later phases, the working-tree scan, `.gitignore` handling, `status`, `add`,
//! and `checkout` (see docs/hlds/working-tree.md). From-scratch — no `gix`/`git2`.
//! This phase is the index codec.

mod checkout;
mod checkout_mode;
mod commit;
mod commit_error;
mod diff;
mod entry;
mod error;
mod excludes;
mod fsmeta;
mod head_reset;
mod ignore;
mod index;
mod index_lock;
mod ls_files;
mod ls_files_options;
mod mv;
mod pathspec;
mod reset;
mod reset_error;
mod restore;
mod rm;
mod sparse;
mod status;
mod submodule;
mod switch;
mod switch_error;
mod worktree;

pub(crate) use checkout_mode::CheckoutMode;
pub use commit_error::CommitError;
pub use diff::{FileDiff, trees as diff_trees};
pub use entry::{IndexEntry, Stat};
pub use error::WorktreeError;
pub use index::{Conflict, Index};
pub use ls_files::{LsFilesConfig, LsFilesOutput};
pub use ls_files_options::LsFilesOptions;
pub use pathspec::PathspecSet;
pub use reset_error::ResetError;
pub use rm::RmOutcome;
pub(crate) use sparse::SparseCheckout;
pub use sparse::{SparseReapply, SparseSet};
pub use status::{Status, StatusEntry, UntrackedEntry};
pub(crate) use submodule::submodule_head_oid;
pub use switch_error::SwitchError;
pub use worktree::{PreparedCommit, ResetMode, ResetOutcome, SwitchOptions, WorkTree};
