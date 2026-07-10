//! Ref listing, resolution, HEAD state, and CAS updates.

use gitana_file_store_local::WorktreeFileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::{HeadState as EngineHeadState, ReflogIntent, Repository};

use crate::bindings::exports::gitana::repo::porcelain::{
	HeadState, RefEntry, ReflogRequest, RepoError, SymbolicHead,
};

use super::repo_error;

/// Map a host-supplied [`ReflogRequest`] to a [`ReflogIntent`]: present means "log this move"
/// (still gated by `core.logAllRefUpdates`), absent means "do not log".
fn reflog_intent(reflog: Option<&ReflogRequest>) -> ReflogIntent<'_> {
	match reflog {
		Some(ReflogRequest { committer, message }) => ReflogIntent::Log { committer, message },
		None => ReflogIntent::Skip,
	}
}

/// Parse a CAS `expected` value: an exact full hex id, never a spec.
fn expected_id<H: HashAlgorithm>(hex: &str) -> Result<ObjectId<H>, RepoError> {
	ObjectId::from_hex(hex)
		.map_err(|_| RepoError::Invalid(format!("expected is not a full object id: {hex}")))
}

pub(crate) async fn list_refs<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	prefix: &str,
) -> Result<Vec<RefEntry>, RepoError> {
	let refs = repo.refs().list(prefix).await.map_err(repo_error)?;
	Ok(
		refs
			.into_iter()
			.map(|(name, id)| RefEntry {
				name,
				id: id.to_hex(),
			})
			.collect(),
	)
}

pub(crate) async fn head<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
) -> Result<HeadState, RepoError> {
	match repo.refs().read_head().await.map_err(repo_error)? {
		EngineHeadState::Symbolic(target) => {
			// Resolve through the symref chain; an unresolvable target is an
			// unborn branch, not an error.
			match repo.refs().resolve_head().await.map_err(repo_error)? {
				Some(id) => Ok(HeadState::Symbolic(SymbolicHead {
					target,
					id: id.to_hex(),
				})),
				None => Ok(HeadState::Unborn(target)),
			}
		}
		EngineHeadState::Detached(id) => Ok(HeadState::Detached(id.to_hex())),
	}
}

pub(crate) async fn resolve_ref<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	name: &str,
) -> Result<Option<String>, RepoError> {
	let id = repo.refs().resolve(name).await.map_err(repo_error)?;
	Ok(id.map(|id| id.to_hex()))
}

pub(crate) async fn update_ref<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	name: &str,
	new: &str,
	expected: Option<&str>,
	reflog: Option<&ReflogRequest>,
) -> Result<(), RepoError> {
	let new = repo.rev_parse(new).await.map_err(repo_error)?;
	let expected = expected.map(expected_id::<H>).transpose()?;
	repo
		.refs()
		.update_ref(name, new, expected, reflog_intent(reflog))
		.await
		.map_err(repo_error)
}

pub(crate) async fn delete_ref<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	name: &str,
	expected: &str,
) -> Result<(), RepoError> {
	let expected = expected_id::<H>(expected)?;
	repo
		.refs()
		.delete_ref(name, Some(expected))
		.await
		.map_err(repo_error)
}

pub(crate) async fn read_symbolic_ref<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	name: &str,
) -> Result<Option<String>, RepoError> {
	repo.refs().read_symbolic(name).await.map_err(repo_error)
}

pub(crate) async fn set_symbolic_ref<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	name: &str,
	target: &str,
	reflog: Option<&ReflogRequest>,
) -> Result<(), RepoError> {
	repo
		.refs()
		.set_symbolic(name, target, reflog_intent(reflog))
		.await
		.map_err(repo_error)
}
