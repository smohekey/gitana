//! Repository-level operations: init layout, config.

use gitana_file_store::FileStore;
use gitana_file_store_local::LocalFileStore;
use gitana_object::HashAlgorithm;
use gitana_repository::{Repository, RepositoryError};

use crate::bindings::exports::gitana::repo::porcelain::{
	RepackReport as WitRepackReport, RepoError,
};

use super::repo_error;

/// git's empty directory skeleton, created by `init` so the repository is
/// recognizable to stock git tooling (the file store itself never creates
/// value-less directories).
const SKELETON: [&str; 5] = [
	"info",
	"objects/info",
	"objects/pack",
	"refs/heads",
	"refs/tags",
];

pub(crate) async fn init_layout(store: &LocalFileStore) -> Result<(), RepoError> {
	for dir in SKELETON {
		store
			.create_dir_all(dir)
			.await
			.map_err(|error| repo_error(RepositoryError::FileStore(error)))?;
	}
	Ok(())
}

/// Write the fresh-repo metadata (idempotent — `write_path_if_absent` under the
/// hood) and validate the resulting config matches the requested hash algorithm:
/// re-initializing a repository of another format fails with `unsupported-format`.
pub(crate) async fn init_repo<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
) -> Result<(), RepoError> {
	repo.init().await.map_err(repo_error)?;
	repo.open().await.map_err(repo_error)?;
	Ok(())
}

/// git's geometric factor, as used by `gta repack --geometric` / `gta gc`.
const GEOMETRIC_FACTOR: u64 = 2;

pub(crate) async fn repack<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
	geometric: bool,
) -> Result<Option<WitRepackReport>, RepoError> {
	let max_pack_size = repo.pack_size_limit().await.map_err(repo_error)?;
	let report = if geometric {
		repo
			.objects()
			.repack_geometric(max_pack_size, GEOMETRIC_FACTOR)
			.await
	} else {
		repo.objects().repack(max_pack_size).await
	}
	.map_err(|error| repo_error(RepositoryError::ObjectStore(error)))?;
	Ok(report.map(|report| WitRepackReport {
		packed_objects: report.packed_objects as u64,
		packs_written: report.packs_written as u64,
		packs_kept: report.packs_kept as u64,
		packs_removed: report.packs_removed as u64,
		loose_removed: report.loose_removed as u64,
	}))
}

pub(crate) async fn read_config<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
) -> Result<String, RepoError> {
	let bytes = repo
		.objects()
		.file_store()
		.read_path("config")
		.await
		.map_err(|error| repo_error(RepositoryError::FileStore(error)))?;
	String::from_utf8(bytes).map_err(|_| RepoError::Invalid("config is not UTF-8".to_owned()))
}
