//! Structured, in-process management of Git **linked worktrees** (`git worktree`), for a library
//! consumer that must not spawn a command-line process.
//!
//! This crate re-expresses Gitana's linked-worktree admin lifecycle — which otherwise lives in the
//! `gta` CLI — as capability-clean library functions returning **matchable** structured data: a
//! repository is identified explicitly ([`RepositoryId`], anchored on the shared common dir, never
//! inferred from a destination path); refusals and conflicts are observations returned inside `Ok` (a
//! [`WorktreeClassification`]), while only genuine failures are [`LinkedWorktreeError`]; object ids
//! cross the boundary as a runtime-tagged [`WorktreeObjectId`] so the caller stays format-agnostic
//! across SHA-1 and SHA-256; identity paths are native `PathBuf` throughout; and nothing here writes to
//! stdout/stderr or reads the process current directory.
//!
//! The read surface is [`inspect`] one destination, [`classify`] its partial state, [`enumerate`] a
//! repository's worktrees, and read a working tree's [`status`]. [`create`] establishes a worktree from an
//! explicit [`CreateRequest`], reconciling against that classification — an idempotent no-op when it
//! already exists exactly, completing an interrupted attempt, and refusing a conflict. [`remove`] is the
//! mirror: a safe, force-free removal from an explicit [`RemoveRequest`] that refuses a dirty/conflicted,
//! locked, primary, or identity-mismatched worktree, retains the branch and its commits, and is idempotent
//! once the worktree is gone.
//!
//! The filesystem-capability mint uses `cap-std`, which does not build for `wasm32`; the reading
//! functions ([`inspect`]/[`enumerate`]/[`status`]) are therefore `cfg(not(target_arch = "wasm32"))`,
//! while the pure types and [`classify`] are available everywhere. A wasm consumer would inject
//! capabilities instead — a later concern (the intended consumer is native).

mod classify;
mod create_error;
mod enumerate;
mod error;
mod facts;
mod inspect;
mod new_worktree_setup;
mod object_id;
mod query;
mod relocate_error;
mod relocate_outcome;
mod relocate_request;
mod remove_error;
mod remove_outcome;
mod remove_policy;
mod remove_request;
mod repo_id;
mod request;
mod setup;
mod status;
mod worktree_context;

// The filesystem-reading helpers exist only to serve the native (cap-std) reading API, so they are
// native-only — on wasm the crate exposes just the pure model + classification.
#[cfg(not(target_arch = "wasm32"))]
mod admin_cleanup;
#[cfg(not(target_arch = "wasm32"))]
mod create;
#[cfg(not(target_arch = "wasm32"))]
mod durability;
#[cfg(not(target_arch = "wasm32"))]
mod head;
#[cfg(not(target_arch = "wasm32"))]
mod initializing_lock;
#[cfg(not(target_arch = "wasm32"))]
mod pointers;
#[cfg(not(target_arch = "wasm32"))]
mod registration_lock;
#[cfg(not(target_arch = "wasm32"))]
mod relocate;
#[cfg(not(target_arch = "wasm32"))]
mod remove;

pub use self::classify::{ProtectionReason, WorktreeClassification, classify};
pub use self::create_error::CreateError;
pub use self::enumerate::{WorktreeEntry, WorktreeListing, WorktreeRole};
pub use self::error::{LinkedWorktreeError, PointerKind};
pub use self::facts::{HeadKind, LockState};
pub use self::inspect::{
	CrossPointerHealth, DestinationKind, HeadFacts, IdentityConflict, Registration, RequestedBranch,
	StartRelation, WorktreeInspection,
};
pub use self::new_worktree_setup::NewWorktreeSetup;
pub use self::object_id::WorktreeObjectId;
pub use self::query::{BranchName, WorktreeQuery};
pub use self::relocate_error::RelocateError;
pub use self::relocate_outcome::RelocateOutcome;
pub use self::relocate_request::RelocateRequest;
pub use self::remove_error::RemoveError;
pub use self::remove_outcome::RemoveOutcome;
pub use self::remove_policy::RemovePolicy;
pub use self::remove_request::RemoveRequest;
pub use self::repo_id::RepositoryId;
pub use self::request::{CheckoutTarget, CreateRequest};
pub use self::setup::Setup;
pub use self::status::{CheckoutSafety, UnpushedRef, WorktreeStatusReport};
pub use self::worktree_context::WorktreeContext;

#[cfg(not(target_arch = "wasm32"))]
pub use self::create::{create, recover_prepared_create};
#[cfg(not(target_arch = "wasm32"))]
pub use self::durability::{durability_barrier_created, durability_barrier_removed};
#[cfg(not(target_arch = "wasm32"))]
pub use self::enumerate::enumerate;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::initializing_lock::InitializingLock;
#[cfg(not(target_arch = "wasm32"))]
pub use self::inspect::inspect;
#[cfg(not(target_arch = "wasm32"))]
pub use self::relocate::relocate;
#[cfg(not(target_arch = "wasm32"))]
pub use self::remove::remove;
#[cfg(not(target_arch = "wasm32"))]
pub use self::status::{checkout_safety, status, unpushed_refs};

// Crate-internal items, re-exported so the rest of the crate names them through the root
// (docs/conventions.md). Native-only, like the code that uses them.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::admin_cleanup::{
	deregister_admin, open_directory_nofollow, path_absent, remove_directory_tree,
	remove_empty_directory,
};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::create::{matches_target, request_query};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::head::{
	read_head, read_lock_reason, structural_head_branch, structural_head_branch_for_move,
};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::inspect::inspect_structural_head;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::object_id::IntoWorktreeObjectId;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::pointers::{
	RefSource, SYMREF_MAXDEPTH, admin_checkout_missing, admin_dirs_for, admin_gitdir_target,
	admin_owned_by, branch_checkout_location, canonical, canonical_eq, checkout_gitfile_names,
	ensure_representable_path, gitfile_target, ignorecase, is_bare, is_leaf_symlink, is_listed_admin,
	linked_admin_dirs, main_checkout_identifies_common, main_worktree_path, os_string_from_bytes,
	path_from_bytes, path_to_bytes, read_worktree_admins, resolve_ref_terminal, resolved_path,
	strip_eol_bytes, update_file_in_place, worktree_path_of, write_file_atomic,
};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::registration_lock::RegistrationLock;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::repo_id::{
	detect_kind, detect_kind_at, open_store_raw, open_work_dir, reject_unsupported_repository_format,
	validate_repository_structure,
};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::status::{
	diverged_tracked_content_paths, first_unreachable_admin_anchor, is_sparse_index,
	partial_has_staged_changes, residual_untracked_paths, status_at,
};
