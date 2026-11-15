//! The git repository engine: git semantics over the storage layer.
//!
//! Composes a repo-scoped [`gitana_object_store::ObjectStore`] (object graph)
//! with refs/HEAD over the file store into a [`Repository`], in a git-compatible
//! sha256 layout (see docs/hlds/repository-engine.md). This phase covers
//! init/open + config + loose refs with CAS; object-graph construction, the
//! reflog, revision resolution, history walk, and packed-refs reading land in
//! later phases.

mod branch_error;
mod branch_name;
mod config;
mod detect;
mod durability;
mod error;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod gated_file_store;
mod head;
mod head_lock;
mod head_reset_plan;
mod head_transaction;
mod history_mutation_lease;
mod initial_commit_transaction;
mod merge;
mod merge_base;
mod merge_state;
mod mode;
mod operation;
mod precondition;
mod prepared_reset;
mod rebase_state;
mod ref_name;
mod ref_op;
mod refs;
mod repository;
mod revision;
mod shallow;
mod tree;

pub use self::{
	branch_error::BranchError,
	branch_name::{BranchName, InvalidBranchName},
	config::Config,
	detect::detect_hash_kind,
	error::RepositoryError,
	head::HeadState,
	head_lock::{HeadLock, PreparedDetachedHead},
	head_transaction::HeadTransaction,
	history_mutation_lease::HistoryMutationLease,
	initial_commit_transaction::InitialCommitTransaction,
	merge::TreeMerge,
	mode::FileMode,
	operation::Operation,
	precondition::Precondition,
	prepared_reset::PreparedReset,
	rebase_state::RebaseState,
	ref_name::{InvalidRefName, RefName},
	ref_op::RefOp,
	refs::{RefStore, ReflogIntent},
	repository::Repository,
	tree::{TreeBuildEntry, compute_tree_id},
};

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::repository::retained_repository_task;
pub(crate) use self::{head_reset_plan::HeadResetPlan, ref_name::is_valid_refname};

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) use self::gated_file_store::GatedFileStore;
