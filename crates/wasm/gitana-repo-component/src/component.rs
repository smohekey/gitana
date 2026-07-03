//! The exported `repository` resource implementation.

use wasip2::filesystem::types::Descriptor;

use crate::bindings::exports::gitana::repo::porcelain::{
	CommitInfo, Guest, GuestRepository, HashKind, HeadState, ObjectInfo, RefEntry, RepackReport,
	RepoError, Repository, TagInfo, TreeBuildEntry, TreeEntry,
};
use crate::inner::Inner;

/// The component itself: wires the exported interface to its resource type.
pub(crate) struct Component;

impl Guest for Component {
	type Repository = GitanaRepository;
}

/// One opened repository per resource instance, owning the descriptor it was granted.
pub(crate) struct GitanaRepository {
	inner: Inner,
}

impl GuestRepository for GitanaRepository {
	fn open(git_dir: Descriptor) -> Result<Repository, RepoError> {
		let inner = Inner::open(git_dir)?;
		Ok(Repository::new(GitanaRepository { inner }))
	}

	fn init(git_dir: Descriptor, kind: HashKind) -> Result<Repository, RepoError> {
		let inner = Inner::init(git_dir, kind)?;
		Ok(Repository::new(GitanaRepository { inner }))
	}

	fn hash_kind(&self) -> HashKind {
		self.inner.hash_kind()
	}

	fn read_config(&self) -> Result<String, RepoError> {
		self.inner.read_config()
	}

	fn read_object(&self, spec: String) -> Result<ObjectInfo, RepoError> {
		self.inner.read_object(&spec)
	}

	fn read_blob(&self, spec: String) -> Result<Vec<u8>, RepoError> {
		self.inner.read_blob(&spec)
	}

	fn read_commit(&self, spec: String) -> Result<CommitInfo, RepoError> {
		self.inner.read_commit(&spec)
	}

	fn read_tag(&self, spec: String) -> Result<TagInfo, RepoError> {
		self.inner.read_tag(&spec)
	}

	fn ls_tree(&self, spec: String) -> Result<Vec<TreeEntry>, RepoError> {
		self.inner.ls_tree(&spec)
	}

	fn rev_parse(&self, spec: String) -> Result<String, RepoError> {
		self.inner.rev_parse(&spec)
	}

	fn rev_list(&self, tips: Vec<String>, max_count: Option<u32>) -> Result<Vec<String>, RepoError> {
		self.inner.rev_list(&tips, max_count)
	}

	fn merge_base(&self, commits: Vec<String>) -> Result<Vec<String>, RepoError> {
		self.inner.merge_base(&commits)
	}

	fn is_ancestor(&self, ancestor: String, descendant: String) -> Result<bool, RepoError> {
		self.inner.is_ancestor(&ancestor, &descendant)
	}

	fn list_refs(&self, prefix: String) -> Result<Vec<RefEntry>, RepoError> {
		self.inner.list_refs(&prefix)
	}

	fn head(&self) -> Result<HeadState, RepoError> {
		self.inner.head()
	}

	fn resolve_ref(&self, name: String) -> Result<Option<String>, RepoError> {
		self.inner.resolve_ref(&name)
	}

	fn update_ref(
		&self,
		name: String,
		new: String,
		expected: Option<String>,
	) -> Result<(), RepoError> {
		self.inner.update_ref(&name, &new, expected.as_deref())
	}

	fn delete_ref(&self, name: String, expected: String) -> Result<(), RepoError> {
		self.inner.delete_ref(&name, &expected)
	}

	fn read_symbolic_ref(&self, name: String) -> Result<Option<String>, RepoError> {
		self.inner.read_symbolic_ref(&name)
	}

	fn set_symbolic_ref(&self, name: String, target: String) -> Result<(), RepoError> {
		self.inner.set_symbolic_ref(&name, &target)
	}

	fn write_blob(&self, data: Vec<u8>) -> Result<String, RepoError> {
		self.inner.write_blob(&data)
	}

	fn repack(&self, geometric: bool) -> Result<Option<RepackReport>, RepoError> {
		self.inner.repack(geometric)
	}

	fn write_tree(&self, entries: Vec<TreeBuildEntry>) -> Result<String, RepoError> {
		self.inner.write_tree(entries)
	}

	fn create_commit(
		&self,
		tree: String,
		parents: Vec<String>,
		author: String,
		committer: String,
		message: String,
	) -> Result<String, RepoError> {
		self
			.inner
			.create_commit(&tree, &parents, &author, &committer, &message)
	}
}
