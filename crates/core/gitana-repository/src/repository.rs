use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::{Commit, ObjectId, ObjectKind, encode_commit, parse_commit};
use gitana_object_store::ObjectStore;

use crate::tree::build_tree;
use crate::{Config, HeadState, RefStore, RepositoryError, TreeBuildEntry};

/// A git repository: the object graph plus refs, over one repo-scoped store.
///
/// The engine is storage-agnostic. The local profile points the file store at a
/// `.git` directory (with a sentinel repo-id) so the on-disk bytes are exactly
/// what `git --object-format=sha256` expects. `init` writes the metadata files
/// (`config`, `HEAD`); creating the empty `objects/`/`refs/` directory skeleton a
/// real git repo needs is a filesystem concern handled by the local wiring.
pub struct Repository<F> {
	objects: ObjectStore<F>,
}

impl<F> Repository<F>
where
	F: FileStore,
{
	/// Wrap a repo-scoped object store as a repository.
	pub fn new(objects: ObjectStore<F>) -> Self {
		Self { objects }
	}

	/// The object store (read/write objects, packs).
	pub fn objects(&self) -> &ObjectStore<F> {
		&self.objects
	}

	/// The ref store (HEAD, branches, tags).
	pub fn refs(&self) -> RefStore<'_, F> {
		RefStore::new(self.objects.file_store())
	}

	/// Write the metadata files for a fresh sha256 repo: `config` and a symbolic
	/// `HEAD → refs/heads/main`. Idempotent — existing files are left untouched.
	pub async fn init(&self) -> Result<(), RepositoryError> {
		let files = self.objects.file_store();

		files
			.write_path_if_absent("config", Config::sha256().render().as_bytes())
			.await?;
		let head = HeadState::Symbolic("refs/heads/main".to_owned()).render();
		files.write_path_if_absent("HEAD", head.as_bytes()).await?;
		Ok(())
	}

	/// Write a blob object, returning its id.
	pub async fn write_blob(&self, data: &[u8]) -> Result<ObjectId, RepositoryError> {
		Ok(self.objects.write_object(ObjectKind::Blob, data).await?)
	}

	/// Build the nested tree objects for `entries` and return the root tree id.
	pub async fn write_tree(&self, entries: &[TreeBuildEntry]) -> Result<ObjectId, RepositoryError> {
		build_tree(&self.objects, entries).await
	}

	/// Recursively read a tree into `(path, mode, oid)` entries (`ls-tree -r`).
	pub async fn read_tree(
		&self,
		tree: ObjectId,
	) -> Result<Vec<crate::tree::FlatEntry>, RepositoryError> {
		crate::tree::read_tree_recursive(&self.objects, tree).await
	}

	/// Read and parse the full git `config` file.
	pub async fn read_config(&self) -> Result<gitana_config::GitConfig, RepositoryError> {
		let bytes = self.objects.file_store().read_path("config").await?;
		let text = std::str::from_utf8(&bytes)
			.map_err(|_| RepositoryError::UnsupportedFormat("config is not UTF-8".to_owned()))?;
		gitana_config::GitConfig::parse(text)
			.map_err(|error| RepositoryError::UnsupportedFormat(error.to_string()))
	}

	/// Read a blob's content.
	pub async fn read_blob(&self, id: ObjectId) -> Result<Vec<u8>, RepositoryError> {
		let (kind, payload) = self.objects.read_object(&id).await?;
		if kind != ObjectKind::Blob {
			return Err(RepositoryError::InvalidRef(format!("{id} is not a blob")));
		}
		Ok(payload)
	}

	/// Read a commit and return the tree it points at.
	pub async fn commit_tree(&self, commit: ObjectId) -> Result<ObjectId, RepositoryError> {
		let (kind, payload) = self.objects.read_object(&commit).await?;
		if kind != ObjectKind::Commit {
			return Err(RepositoryError::InvalidRef(format!(
				"{commit} is not a commit"
			)));
		}
		Ok(parse_commit(&payload)?.tree)
	}

	/// Write a commit object (no ref update), returning its id. `author` and
	/// `committer` are git identity lines (`Name <email> seconds ±hhmm`).
	pub async fn create_commit(
		&self,
		tree: ObjectId,
		parents: Vec<ObjectId>,
		author: &str,
		committer: &str,
		message: &str,
	) -> Result<ObjectId, RepositoryError> {
		let commit = Commit {
			tree,
			parents,
			author: author.to_owned(),
			committer: committer.to_owned(),
			signature: None,
			message: message.to_owned(),
		};
		Ok(
			self
				.objects
				.write_object(ObjectKind::Commit, &encode_commit(&commit))
				.await?,
		)
	}

	/// Create a commit on the branch `HEAD` points at, advancing the branch via CAS
	/// and appending reflog entries to the branch and `HEAD`. Returns the commit id.
	/// Detached HEAD is not yet supported.
	pub async fn commit_on_head(
		&self,
		tree: ObjectId,
		author: &str,
		committer: &str,
		message: &str,
	) -> Result<ObjectId, RepositoryError> {
		let refs = self.refs();
		let target = match refs.read_head().await? {
			HeadState::Symbolic(target) => target,
			HeadState::Detached(_) => {
				return Err(RepositoryError::Unsupported(
					"commit on detached HEAD".to_owned(),
				));
			}
		};

		let parent = refs.resolve(&target).await?;
		let parents = parent.map(|p| vec![p]).unwrap_or_default();
		let commit = self
			.create_commit(tree, parents, author, committer, message)
			.await?;
		refs.update_ref(&target, commit, parent).await?;

		let subject = message.lines().next().unwrap_or("");
		let reflog = if parent.is_none() {
			format!("commit (initial): {subject}")
		} else {
			format!("commit: {subject}")
		};
		refs
			.append_reflog(&target, parent, commit, committer, &reflog)
			.await?;
		refs
			.append_reflog("HEAD", parent, commit, committer, &reflog)
			.await?;
		Ok(commit)
	}

	/// Move the current branch (or detached `HEAD`) to `commit` via CAS, recording the previous
	/// tip in `ORIG_HEAD` and appending a reflog entry (`message`, e.g. `reset: moving to
	/// HEAD~1`) to the branch and `HEAD`. The index and working tree are not touched. Mirrors the
	/// ref half of [`Self::commit_on_head`], but for a reset rather than a new commit.
	pub async fn reset_head(
		&self,
		commit: ObjectId,
		committer: &str,
		message: &str,
	) -> Result<(), RepositoryError> {
		let refs = self.refs();
		let head = refs.read_head().await?;
		let old = match &head {
			HeadState::Symbolic(branch) => refs.resolve(branch).await?,
			HeadState::Detached(id) => Some(*id),
		};

		// Record the pre-reset tip so `gta reset ORIG_HEAD` can recover it, as git does. There is
		// nothing to record (and nothing to move from) on an unborn branch.
		if let Some(old) = old {
			let current = refs.resolve("ORIG_HEAD").await?;
			refs.update_ref("ORIG_HEAD", old, current).await?;
		}

		match head {
			HeadState::Symbolic(branch) => {
				refs.update_ref(&branch, commit, old).await?;
				refs
					.append_reflog(&branch, old, commit, committer, message)
					.await?;
			}
			HeadState::Detached(_) => {
				refs.update_ref("HEAD", commit, old).await?;
			}
		}
		refs
			.append_reflog("HEAD", old, commit, committer, message)
			.await?;
		Ok(())
	}

	/// Resolve a revision spec (`HEAD`, `main`, `<oid>`, `HEAD~2`, `v1^{commit}`, …)
	/// to an object id.
	pub async fn rev_parse(&self, spec: &str) -> Result<ObjectId, RepositoryError> {
		crate::revision::rev_parse(self, spec).await
	}

	/// Walk commits reachable from `tips` in committer-date order (newest first).
	pub async fn rev_list(&self, tips: &[ObjectId]) -> Result<Vec<ObjectId>, RepositoryError> {
		crate::revision::rev_list(self, tips).await
	}

	/// Read and validate the repository config; refuse non-sha256 / unknown formats.
	pub async fn open(&self) -> Result<Config, RepositoryError> {
		let files = self.objects.file_store();

		let bytes = match files.read_path("config").await {
			Ok(bytes) => bytes,
			Err(FileStoreError::NotFound) => {
				return Err(RepositoryError::UnsupportedFormat(
					"no config file".to_owned(),
				));
			}
			Err(other) => return Err(other.into()),
		};
		let text = std::str::from_utf8(&bytes)
			.map_err(|_| RepositoryError::UnsupportedFormat("config is not UTF-8".to_owned()))?;
		Config::parse(text)
	}
}
