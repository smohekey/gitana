//! Revision resolution and history walks.

use gitana_file_store_local::LocalFileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

use crate::bindings::exports::gitana::repo::porcelain::RepoError;

use super::repo_error;

/// Resolve each spec in `specs` to an id, failing on the first unresolvable one.
async fn resolve_all<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
	specs: &[String],
) -> Result<Vec<ObjectId<H>>, RepoError> {
	let mut ids = Vec::with_capacity(specs.len());
	for spec in specs {
		ids.push(repo.rev_parse(spec).await.map_err(repo_error)?);
	}
	Ok(ids)
}

pub(crate) async fn rev_parse<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
	spec: &str,
) -> Result<String, RepoError> {
	let id = repo.rev_parse(spec).await.map_err(repo_error)?;
	Ok(id.to_hex())
}

pub(crate) async fn rev_list<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
	tips: &[String],
	max_count: Option<u32>,
) -> Result<Vec<String>, RepoError> {
	let tips = resolve_all(repo, tips).await?;
	let mut commits = repo.rev_list(&tips).await.map_err(repo_error)?;
	if let Some(max) = max_count {
		commits.truncate(max as usize);
	}
	Ok(commits.iter().map(|id| id.to_hex()).collect())
}

pub(crate) async fn merge_base<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
	commits: &[String],
) -> Result<Vec<String>, RepoError> {
	let commits = resolve_all(repo, commits).await?;
	let bases = repo.merge_base(&commits).await.map_err(repo_error)?;
	Ok(bases.iter().map(|id| id.to_hex()).collect())
}

pub(crate) async fn is_ancestor<H: HashAlgorithm>(
	repo: &Repository<LocalFileStore, H>,
	ancestor: &str,
	descendant: &str,
) -> Result<bool, RepoError> {
	let ancestor = repo.rev_parse(ancestor).await.map_err(repo_error)?;
	let descendant = repo.rev_parse(descendant).await.map_err(repo_error)?;
	repo
		.is_ancestor(ancestor, descendant)
		.await
		.map_err(repo_error)
}
