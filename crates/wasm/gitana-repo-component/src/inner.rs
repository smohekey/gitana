//! Runtime hash-kind dispatch: one repository value per compile-time `H`.

use gitana_file_store::FileStoreError;
use gitana_file_store_local::LocalFileStore;
use gitana_object::{HashAlgorithm, ObjectKind, Sha1, Sha256, parse_commit};
use gitana_object_store::{ObjectStore, ObjectStoreError};
use gitana_repository::{Repository, RepositoryError, detect_hash_kind};
use wasip2::filesystem::types::Descriptor;

use crate::bindings::exports::gitana::repo::porcelain::{CommitInfo, HashKind, RepoError};
use crate::block_on::block_on;

/// The repository under its runtime-detected hash algorithm. The same
/// runtime→compile-time bridge as `gta-core`'s dispatch: match once here, and each
/// operation body is written once, generic over `H`.
pub(crate) enum Inner {
	Sha1(Repository<LocalFileStore, Sha1>),
	Sha256(Repository<LocalFileStore, Sha256>),
}

impl Inner {
	/// Build the file store over the granted descriptor, detect the object format from
	/// `config` *through that descriptor*, and open the repository as the matching `H`.
	pub(crate) fn open(git_dir: Descriptor) -> Result<Self, RepoError> {
		let store = LocalFileStore::from_descriptor(git_dir);
		match block_on(detect_hash_kind(&store)).map_err(repo_error)? {
			gitana_object::HashKind::Sha1 => Ok(Self::Sha1(Repository::new(ObjectStore::new(store)))),
			gitana_object::HashKind::Sha256 => Ok(Self::Sha256(Repository::new(ObjectStore::new(store)))),
		}
	}

	pub(crate) fn hash_kind(&self) -> HashKind {
		match self {
			Self::Sha1(_) => HashKind::Sha1,
			Self::Sha256(_) => HashKind::Sha256,
		}
	}

	pub(crate) fn read_commit(&self, spec: &str) -> Result<CommitInfo, RepoError> {
		match self {
			Self::Sha1(repo) => block_on(read_commit(repo, spec)),
			Self::Sha256(repo) => block_on(read_commit(repo, spec)),
		}
	}

	pub(crate) fn write_blob(&self, data: &[u8]) -> Result<String, RepoError> {
		match self {
			Self::Sha1(repo) => block_on(write_blob(repo, data)),
			Self::Sha256(repo) => block_on(write_blob(repo, data)),
		}
	}
}

async fn read_commit<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
	spec: &str,
) -> Result<CommitInfo, RepoError> {
	let id = repo.rev_parse(spec).await.map_err(repo_error)?;
	let (kind, payload) = repo
		.objects()
		.read_object(&id)
		.await
		.map_err(|error| repo_error(RepositoryError::ObjectStore(error)))?;
	if kind != ObjectKind::Commit {
		return Err(RepoError::Invalid(format!(
			"{spec} is a {}, not a commit",
			kind.as_str()
		)));
	}
	let commit =
		parse_commit::<H>(&payload).map_err(|error| repo_error(RepositoryError::Object(error)))?;
	Ok(CommitInfo {
		id: id.to_hex(),
		tree: commit.tree.to_hex(),
		parents: commit
			.parents
			.iter()
			.map(|parent| parent.to_hex())
			.collect(),
		author: commit.author,
		committer: commit.committer,
		message: commit.message,
	})
}

async fn write_blob<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
	data: &[u8],
) -> Result<String, RepoError> {
	let id = repo.write_blob(data).await.map_err(repo_error)?;
	Ok(id.to_hex())
}

/// Map engine errors onto the WIT error surface. Coarse for the spike: `invalid-ref`
/// covers both malformed and unresolved specs today, so only genuinely absent
/// objects/files map to `not-found`.
fn repo_error(error: RepositoryError) -> RepoError {
	match error {
		RepositoryError::MissingObject(id) => RepoError::NotFound(format!("missing object {id}")),
		RepositoryError::FileStore(FileStoreError::NotFound)
		| RepositoryError::ObjectStore(ObjectStoreError::NotFound) => {
			RepoError::NotFound("not found".to_owned())
		}
		RepositoryError::InvalidRef(message) => RepoError::Invalid(message),
		RepositoryError::UnsupportedFormat(message) => RepoError::Invalid(message),
		RepositoryError::Object(error) => RepoError::Invalid(error.to_string()),
		other => RepoError::Backend(other.to_string()),
	}
}
