//! The working tree and git index (staging area).
//!
//! Owns a git-compatible `.git/index` (DIRC, read v2–v4 / write v4, sha256) and,
//! in later phases, the working-tree scan, `.gitignore` handling, `status`, `add`,
//! and `checkout` (see docs/hlds/working-tree.md). From-scratch — no `gix`/`git2`.
//! This phase is the index codec.

mod checkout;
mod diff;
mod entry;
mod error;
mod fsmeta;
mod ignore;
mod index;
mod mv;
mod pathspec;
mod reset;
mod restore;
mod rm;
mod status;
mod worktree;

pub use diff::FileDiff;
pub use entry::{IndexEntry, Stat};
pub use error::WorktreeError;
pub use index::{Conflict, Index};
pub use rm::RmOutcome;
pub use status::{Status, StatusEntry};
pub use worktree::WorkTree;
