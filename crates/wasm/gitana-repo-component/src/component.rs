//! The exported `repository` resource implementation.

use wasip2::filesystem::types::Descriptor;

use crate::bindings::exports::gitana::repo::porcelain::{
	CommitInfo, Guest, GuestRepository, HashKind, RepoError, Repository,
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

	fn hash_kind(&self) -> HashKind {
		self.inner.hash_kind()
	}

	fn read_commit(&self, spec: String) -> Result<CommitInfo, RepoError> {
		self.inner.read_commit(&spec)
	}

	fn write_blob(&self, data: Vec<u8>) -> Result<String, RepoError> {
		self.inner.write_blob(&data)
	}
}
