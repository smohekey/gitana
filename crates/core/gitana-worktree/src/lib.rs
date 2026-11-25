//! The working tree and git index (staging area).
//!
//! Owns a git-compatible `.git/index` (DIRC, read v2–v4 / write v4, sha256) and,
//! in later phases, the working-tree scan, `.gitignore` handling, `status`, `add`,
//! and `checkout` (see docs/hlds/working-tree.md). From-scratch — no `gix`/`git2`.
//! This phase is the index codec.

mod attr_direction;
mod attr_file;
mod attr_state;
mod attributes;
mod checkout;
mod checkout_mode;
mod commit;
mod commit_error;
mod conflict_overwrite_error;
mod conflicted_state;
mod converter;
mod crlf_action;
mod diff;
mod entry;
mod eol_config;
mod error;
mod excludes;
mod fsmeta;
mod head_reset;
mod ignore;
mod ignore_submodules;
mod index;
mod index_lock;
mod line_ending_change;
mod line_endings;
mod ls_files;
mod ls_files_options;
mod mv;
mod outside_attributes;
mod pathspec;
mod reset;
mod reset_error;
mod restore;
mod rm;
mod sparse;
mod stash;
mod stash_applied;
mod stash_apply_error;
mod stash_pop_error;
mod stash_push_error;
mod status;
mod submodule;
mod submodule_probe;
mod switch;
mod switch_error;
mod text_stats;
mod worktree;

pub(crate) use attr_direction::AttrDirection;
pub(crate) use attr_file::AttrFile;
pub(crate) use attr_state::AttrState;
pub(crate) use attributes::Attributes;
pub(crate) use checkout_mode::CheckoutMode;
pub use commit_error::CommitError;
pub use conflict_overwrite_error::ConflictOverwriteError;
pub(crate) use converter::{Converter, Staged};
pub(crate) use crlf_action::CrlfAction;
pub use diff::{FileDiff, trees as diff_trees};
pub use entry::{IndexEntry, Stat};
pub(crate) use eol_config::{AutoCrlf, EolConfig, SafeCrlf};
pub use error::WorktreeError;
pub use ignore_submodules::IgnoreSubmodules;
pub use index::{Conflict, Index};
pub use line_ending_change::LineEndingChange;
pub use ls_files::{LsFilesConfig, LsFilesOutput};
pub use ls_files_options::LsFilesOptions;
pub use outside_attributes::OutsideAttributes;
pub use pathspec::PathspecSet;
pub use reset_error::ResetError;
pub use rm::RmOutcome;
pub(crate) use sparse::SparseCheckout;
pub use sparse::{SparseReapply, SparseSet};
pub use stash_applied::StashApplied;
pub use stash_apply_error::StashApplyError;
pub use stash_pop_error::StashPopError;
pub use stash_push_error::StashPushError;
pub use status::{Status, StatusEntry, SubmoduleState, UntrackedEntry};
pub(crate) use submodule::submodule_head_oid;
pub use submodule_probe::{SubmoduleProbe, SubmoduleReport};
pub use switch_error::SwitchError;
pub(crate) use text_stats::TextStats;
pub use worktree::{
	AddOutcome, PreparedCommit, ResetMode, ResetOutcome, StashApply, StashPush, StashPushed,
	SwitchOptions, WorkTree,
};
