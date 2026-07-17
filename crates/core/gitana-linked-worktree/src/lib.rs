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
mod object_id;
mod query;
mod remove_error;
mod remove_outcome;
mod remove_request;
mod repo_id;
mod request;
mod status;
mod worktree_context;

// The filesystem-reading helpers exist only to serve the native (cap-std) reading API, so they are
// native-only — on wasm the crate exposes just the pure model + classification.
#[cfg(not(target_arch = "wasm32"))]
mod create;
#[cfg(not(target_arch = "wasm32"))]
mod head;
#[cfg(not(target_arch = "wasm32"))]
mod pointers;
#[cfg(not(target_arch = "wasm32"))]
mod registration_lock;
#[cfg(not(target_arch = "wasm32"))]
mod remove;

pub use classify::{ProtectionReason, WorktreeClassification, classify};
pub use create_error::CreateError;
pub use enumerate::{WorktreeEntry, WorktreeListing, WorktreeRole};
pub use error::{LinkedWorktreeError, PointerKind};
pub use facts::{HeadKind, LockState};
pub use inspect::{
	CrossPointerHealth, DestinationKind, HeadFacts, IdentityConflict, Registration, RequestedBranch,
	StartRelation, WorktreeInspection,
};
pub use object_id::WorktreeObjectId;
pub use query::{BranchName, WorktreeQuery};
pub use remove_error::RemoveError;
pub use remove_outcome::RemoveOutcome;
pub use remove_request::RemoveRequest;
pub use repo_id::RepositoryId;
pub use request::{CheckoutTarget, CreateRequest};
pub use status::WorktreeStatusReport;
pub use worktree_context::WorktreeContext;

#[cfg(not(target_arch = "wasm32"))]
pub use create::create;
#[cfg(not(target_arch = "wasm32"))]
pub use enumerate::enumerate;
#[cfg(not(target_arch = "wasm32"))]
pub use inspect::inspect;
#[cfg(not(target_arch = "wasm32"))]
pub use remove::remove;
#[cfg(not(target_arch = "wasm32"))]
pub use status::status;
