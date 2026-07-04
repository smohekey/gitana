//! Object reads and writes.

use gitana_file_store_local::WorktreeFileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, parse_commit, parse_tag};
use gitana_object_store::ObjectStoreError;
use gitana_repository::{FileMode, Repository, RepositoryError, TreeBuildEntry};

use crate::bindings::exports::gitana::repo::porcelain::{
	CommitInfo, ObjectInfo, RepoError, TagInfo, TreeEntry,
};
use crate::bindings::exports::gitana::repo::porcelain::{
	FileMode as WitFileMode, ObjectKind as WitObjectKind, TreeBuildEntry as WitTreeBuildEntry,
};

use super::repo_error;

/// Resolve `spec` and require the object it names to be of `kind`.
async fn resolve_kind<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	spec: &str,
	kind: ObjectKind,
) -> Result<ObjectId<H>, RepoError> {
	let id = repo.rev_parse(spec).await.map_err(repo_error)?;
	let (actual, _) = repo
		.objects()
		.read_object(&id)
		.await
		.map_err(|error| repo_error(RepositoryError::ObjectStore(error)))?;
	if actual != kind {
		return Err(RepoError::Invalid(format!(
			"{spec} is a {}, not a {}",
			actual.as_str(),
			kind.as_str()
		)));
	}
	Ok(id)
}

/// The WIT counterpart of an engine [`ObjectKind`].
fn wit_kind(kind: ObjectKind) -> WitObjectKind {
	match kind {
		ObjectKind::Blob => WitObjectKind::Blob,
		ObjectKind::Tree => WitObjectKind::Tree,
		ObjectKind::Commit => WitObjectKind::Commit,
		ObjectKind::Tag => WitObjectKind::Tag,
	}
}

pub(crate) async fn read_object<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	spec: &str,
) -> Result<ObjectInfo, RepoError> {
	let id = repo.rev_parse(spec).await.map_err(repo_error)?;
	let (kind, payload) = repo
		.objects()
		.read_object(&id)
		.await
		.map_err(|error| repo_error(RepositoryError::ObjectStore(error)))?;
	Ok(ObjectInfo {
		id: id.to_hex(),
		kind: wit_kind(kind),
		payload,
	})
}

pub(crate) async fn read_blob<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	spec: &str,
) -> Result<Vec<u8>, RepoError> {
	let id = repo.rev_parse(spec).await.map_err(repo_error)?;
	repo.read_blob(id).await.map_err(repo_error)
}

pub(crate) async fn read_tag<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	spec: &str,
) -> Result<TagInfo, RepoError> {
	let id = repo.rev_parse(spec).await.map_err(repo_error)?;
	let (kind, payload) = repo
		.objects()
		.read_object(&id)
		.await
		.map_err(|error| repo_error(RepositoryError::ObjectStore(error)))?;
	if kind != ObjectKind::Tag {
		return Err(RepoError::Invalid(format!(
			"{spec} is a {}, not a tag",
			kind.as_str()
		)));
	}
	let tag = parse_tag::<H>(&payload).map_err(|error| repo_error(RepositoryError::Object(error)))?;
	Ok(TagInfo {
		id: id.to_hex(),
		target: tag.object.to_hex(),
		target_kind: wit_kind(tag.kind),
		name: tag.name,
		tagger: tag.tagger,
		message: tag.message,
	})
}

pub(crate) async fn ls_tree<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	spec: &str,
) -> Result<Vec<TreeEntry>, RepoError> {
	let id = repo.rev_parse(spec).await.map_err(repo_error)?;
	let tree = repo.peel_to_tree(id).await.map_err(repo_error)?;
	let entries = repo.read_tree(tree).await.map_err(repo_error)?;
	Ok(
		entries
			.into_iter()
			.map(|(path, mode, id)| TreeEntry {
				path,
				mode,
				id: id.to_hex(),
			})
			.collect(),
	)
}

pub(crate) async fn read_commit<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
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

pub(crate) async fn write_blob<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	data: &[u8],
) -> Result<String, RepoError> {
	let id = repo.write_blob(data).await.map_err(repo_error)?;
	Ok(id.to_hex())
}

/// Lexically validate a `write-tree` entry path: `/`-separated, no empty, `.`,
/// `..`, or NUL-carrying components (NUL is the tree codec's separator).
fn validate_tree_path(path: &str) -> Result<(), RepoError> {
	let valid = !path.is_empty()
		&& path
			.split('/')
			.all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\0'));
	if valid {
		Ok(())
	} else {
		Err(RepoError::Invalid(format!("invalid tree path: {path:?}")))
	}
}

pub(crate) async fn write_tree<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	entries: Vec<WitTreeBuildEntry>,
) -> Result<String, RepoError> {
	// Validate the path *set* first: duplicates, or one path serving as both a
	// file and a directory, would encode a tree `git fsck` rejects.
	let mut paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
	let mut dirs: std::collections::HashSet<&str> = std::collections::HashSet::new();
	for entry in &entries {
		validate_tree_path(&entry.path)?;
		if !paths.insert(&entry.path) {
			return Err(RepoError::Invalid(format!(
				"duplicate tree path: {}",
				entry.path
			)));
		}
		let mut rest = entry.path.as_str();
		while let Some((dir, _)) = rest.rsplit_once('/') {
			dirs.insert(dir);
			rest = dir;
		}
	}
	if let Some(conflict) = paths.iter().find(|path| dirs.contains(*path)) {
		return Err(RepoError::Invalid(format!(
			"tree path {conflict} is both a file and a directory"
		)));
	}

	let mut converted = Vec::with_capacity(entries.len());
	for entry in entries {
		let id = ObjectId::from_hex(&entry.id)
			.map_err(|_| RepoError::Invalid(format!("not a full object id: {}", entry.id)))?;
		// Every write-tree mode names a blob; a dangling or non-blob id would
		// produce a tree git tooling cannot consume.
		let (kind, _) = match repo.objects().read_object(&id).await {
			Ok(object) => object,
			Err(ObjectStoreError::NotFound) => {
				return Err(RepoError::Invalid(format!(
					"tree entry {}: no such object {}",
					entry.path, entry.id
				)));
			}
			Err(error) => return Err(repo_error(RepositoryError::ObjectStore(error))),
		};
		if kind != ObjectKind::Blob {
			return Err(RepoError::Invalid(format!(
				"tree entry {}: {} is a {}, not a blob",
				entry.path,
				entry.id,
				kind.as_str()
			)));
		}
		let mode = match entry.mode {
			WitFileMode::Regular => FileMode::Regular,
			WitFileMode::Executable => FileMode::Executable,
			WitFileMode::Symlink => FileMode::Symlink,
		};
		converted.push(TreeBuildEntry {
			path: entry.path,
			mode,
			id,
		});
	}
	let id = repo.write_tree(&converted).await.map_err(repo_error)?;
	Ok(id.to_hex())
}

pub(crate) async fn create_commit<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	tree: &str,
	parents: &[String],
	author: &str,
	committer: &str,
	message: &str,
) -> Result<String, RepoError> {
	let tree = resolve_kind(repo, tree, ObjectKind::Tree).await?;
	let mut parent_ids = Vec::with_capacity(parents.len());
	for parent in parents {
		parent_ids.push(resolve_kind(repo, parent, ObjectKind::Commit).await?);
	}
	let id = repo
		.create_commit(tree, parent_ids, author, committer, message)
		.await
		.map_err(repo_error)?;
	Ok(id.to_hex())
}
