//! The git repository engine: git semantics over the storage layer.
//!
//! Composes a repo-scoped [`gitana_object_store::ObjectStore`] (object graph)
//! with refs/HEAD over the file store into a [`Repository`], in a git-compatible
//! sha256 layout (see docs/hlds/repository-engine.md). This phase covers
//! init/open + config + loose refs with CAS; object-graph construction, the
//! reflog, revision resolution, history walk, and packed-refs reading land in
//! later phases.

mod config;
mod detect;
mod error;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod gated_file_store;
mod head;
mod merge;
mod merge_base;
mod merge_state;
mod mode;
mod rebase_state;
mod ref_op;
mod refs;
mod repository;
mod revision;
mod shallow;
mod tree;

pub use self::{
	config::Config,
	detect::detect_hash_kind,
	error::RepositoryError,
	head::HeadState,
	merge::TreeMerge,
	mode::FileMode,
	rebase_state::RebaseState,
	ref_op::RefOp,
	refs::{RefStore, ReflogIntent},
	repository::Repository,
	tree::{TreeBuildEntry, compute_tree_id},
};

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) use self::gated_file_store::GatedFileStore;
