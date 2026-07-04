//! Runtime hash-kind dispatch: one repository value per compile-time `H`.

use gitana_file_store_local::{LocalFileStore, WorktreeFileStore};
use gitana_object::{Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::{Repository, detect_hash_kind};
use wasip2::filesystem::types::Descriptor;

use crate::bindings::exports::gitana::repo::porcelain::{
	CommitInfo, HashKind, HeadState, ObjectInfo, RefEntry, RepackReport, RepoError, TagInfo,
	TreeBuildEntry, TreeEntry,
};
use crate::block_on::block_on;
use crate::ops;

/// Run `body` once with `repo` bound to whichever concrete `Repository<_, H>` this
/// value holds — the two-arm runtime→compile-time bridge, written once.
macro_rules! dispatch {
	($self:expr, $repo:ident => $body:expr) => {
		match $self {
			Self::Sha1($repo) => $body,
			Self::Sha256($repo) => $body,
		}
	};
}

/// The repository under its runtime-detected hash algorithm. The same
/// runtime→compile-time bridge as `gta-core`'s dispatch: match once here, and each
/// operation body is written once, generic over `H` (see [`crate::ops`]).
pub(crate) enum Inner {
	Sha1(Repository<WorktreeFileStore, Sha1>),
	Sha256(Repository<WorktreeFileStore, Sha256>),
}

impl Inner {
	/// Open the repository whose git dir *is* its common dir (an ordinary,
	/// non-linked repository) over the single granted descriptor.
	pub(crate) fn open(git_dir: Descriptor) -> Result<Self, RepoError> {
		Self::from_store(WorktreeFileStore::single(LocalFileStore::from_descriptor(
			git_dir,
		)))
	}

	/// Open a linked worktree, routing per-worktree paths to `git_dir` and shared
	/// paths (objects, refs, `packed-refs`, `config`) to `common_dir`.
	pub(crate) fn open_worktree(
		git_dir: Descriptor,
		common_dir: Descriptor,
	) -> Result<Self, RepoError> {
		Self::from_store(WorktreeFileStore::from_stores(
			LocalFileStore::from_descriptor(common_dir),
			LocalFileStore::from_descriptor(git_dir),
		))
	}

	/// Detect the object format from `config` *through the store* (which reads it from
	/// the common dir) and open the repository as the matching `H`.
	fn from_store(store: WorktreeFileStore) -> Result<Self, RepoError> {
		match block_on(detect_hash_kind(&store)).map_err(ops::repo_error)? {
			gitana_object::HashKind::Sha1 => Ok(Self::Sha1(Repository::new(ObjectStore::new(store)))),
			gitana_object::HashKind::Sha256 => Ok(Self::Sha256(Repository::new(ObjectStore::new(store)))),
		}
	}

	/// Lay out git's directory skeleton, write the fresh-repo metadata for the
	/// *requested* algorithm (idempotent), and open — refusing a directory that
	/// already holds a repository of a different format. A fresh repository is never
	/// linked, so its git dir and common dir coincide.
	pub(crate) fn init(git_dir: Descriptor, kind: HashKind) -> Result<Self, RepoError> {
		let store = LocalFileStore::from_descriptor(git_dir);
		block_on(ops::init_layout(&store))?;
		let store = WorktreeFileStore::single(store);
		let inner = match kind {
			HashKind::Sha1 => Self::Sha1(Repository::new(ObjectStore::new(store))),
			HashKind::Sha256 => Self::Sha256(Repository::new(ObjectStore::new(store))),
		};
		dispatch!(&inner, repo => block_on(ops::init_repo(repo)))?;
		Ok(inner)
	}

	pub(crate) fn hash_kind(&self) -> HashKind {
		match self {
			Self::Sha1(_) => HashKind::Sha1,
			Self::Sha256(_) => HashKind::Sha256,
		}
	}

	pub(crate) fn read_config(&self) -> Result<String, RepoError> {
		dispatch!(self, repo => block_on(ops::read_config(repo)))
	}

	pub(crate) fn read_object(&self, spec: &str) -> Result<ObjectInfo, RepoError> {
		dispatch!(self, repo => block_on(ops::read_object(repo, spec)))
	}

	pub(crate) fn read_blob(&self, spec: &str) -> Result<Vec<u8>, RepoError> {
		dispatch!(self, repo => block_on(ops::read_blob(repo, spec)))
	}

	pub(crate) fn read_commit(&self, spec: &str) -> Result<CommitInfo, RepoError> {
		dispatch!(self, repo => block_on(ops::read_commit(repo, spec)))
	}

	pub(crate) fn read_tag(&self, spec: &str) -> Result<TagInfo, RepoError> {
		dispatch!(self, repo => block_on(ops::read_tag(repo, spec)))
	}

	pub(crate) fn ls_tree(&self, spec: &str) -> Result<Vec<TreeEntry>, RepoError> {
		dispatch!(self, repo => block_on(ops::ls_tree(repo, spec)))
	}

	pub(crate) fn rev_parse(&self, spec: &str) -> Result<String, RepoError> {
		dispatch!(self, repo => block_on(ops::rev_parse(repo, spec)))
	}

	pub(crate) fn rev_list(
		&self,
		tips: &[String],
		max_count: Option<u32>,
	) -> Result<Vec<String>, RepoError> {
		dispatch!(self, repo => block_on(ops::rev_list(repo, tips, max_count)))
	}

	pub(crate) fn merge_base(&self, commits: &[String]) -> Result<Vec<String>, RepoError> {
		dispatch!(self, repo => block_on(ops::merge_base(repo, commits)))
	}

	pub(crate) fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, RepoError> {
		dispatch!(self, repo => block_on(ops::is_ancestor(repo, ancestor, descendant)))
	}

	pub(crate) fn list_refs(&self, prefix: &str) -> Result<Vec<RefEntry>, RepoError> {
		dispatch!(self, repo => block_on(ops::list_refs(repo, prefix)))
	}

	pub(crate) fn head(&self) -> Result<HeadState, RepoError> {
		dispatch!(self, repo => block_on(ops::head(repo)))
	}

	pub(crate) fn resolve_ref(&self, name: &str) -> Result<Option<String>, RepoError> {
		dispatch!(self, repo => block_on(ops::resolve_ref(repo, name)))
	}

	pub(crate) fn update_ref(
		&self,
		name: &str,
		new: &str,
		expected: Option<&str>,
	) -> Result<(), RepoError> {
		dispatch!(self, repo => block_on(ops::update_ref(repo, name, new, expected)))
	}

	pub(crate) fn delete_ref(&self, name: &str, expected: &str) -> Result<(), RepoError> {
		dispatch!(self, repo => block_on(ops::delete_ref(repo, name, expected)))
	}

	pub(crate) fn read_symbolic_ref(&self, name: &str) -> Result<Option<String>, RepoError> {
		dispatch!(self, repo => block_on(ops::read_symbolic_ref(repo, name)))
	}

	pub(crate) fn set_symbolic_ref(&self, name: &str, target: &str) -> Result<(), RepoError> {
		dispatch!(self, repo => block_on(ops::set_symbolic_ref(repo, name, target)))
	}

	pub(crate) fn write_blob(&self, data: &[u8]) -> Result<String, RepoError> {
		dispatch!(self, repo => block_on(ops::write_blob(repo, data)))
	}

	pub(crate) fn repack(&self, geometric: bool) -> Result<Option<RepackReport>, RepoError> {
		dispatch!(self, repo => block_on(ops::repack(repo, geometric)))
	}

	pub(crate) fn write_tree(&self, entries: Vec<TreeBuildEntry>) -> Result<String, RepoError> {
		dispatch!(self, repo => block_on(ops::write_tree(repo, entries)))
	}

	pub(crate) fn create_commit(
		&self,
		tree: &str,
		parents: &[String],
		author: &str,
		committer: &str,
		message: &str,
	) -> Result<String, RepoError> {
		dispatch!(self, repo => block_on(ops::create_commit(repo, tree, parents, author, committer, message)))
	}
}
