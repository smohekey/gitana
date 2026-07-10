//! Runtime hash-kind dispatch: one repository value per compile-time `H`.

use gitana_file_store_local::{DescriptorWorkDir, LocalFileStore, WorktreeFileStore};
use gitana_object::{HashAlgorithm, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_remote::Origin;
use gitana_repository::{Repository, detect_hash_kind};
use gitana_worktree::WorkTree;
use wasip2::filesystem::types::Descriptor;

use crate::bindings::exports::gitana::repo::porcelain::{
	CommitInfo, FetchOutcome, HashKind, HeadState, ObjectInfo, PushOutcome, RefEntry, ReflogRequest,
	RepackReport, RepoError, TagInfo, TreeBuildEntry, TreeEntry, WorktreeStatus,
};
use crate::block_on::block_on;
use crate::ops;

/// What a repository resource holds for one compile-time `H`: either a plumbing-only
/// [`Repository`] (opened via `open`/`init`, no working tree) or a full [`WorkTree`]
/// (opened via `open-worktree`, which owns the repository and adds the work-dir
/// capability). Plumbing operations reach the repository through [`Held::repository`]
/// either way; working-tree operations require [`Held::worktree`].
pub(crate) enum Held<H: HashAlgorithm> {
	/// No working tree — `open`/`init`. Only plumbing operations are available.
	Plumbing(Repository<WorktreeFileStore, H>),
	/// A working tree over the wasm descriptor capability — `open-worktree`.
	Worktree(WorkTree<WorktreeFileStore, DescriptorWorkDir, H>),
}

impl<H: HashAlgorithm> Held<H> {
	/// The repository, whichever shape backs it (a `WorkTree` owns its repository).
	fn repository(&self) -> &Repository<WorktreeFileStore, H> {
		match self {
			Held::Plumbing(repo) => repo,
			Held::Worktree(wt) => wt.repository(),
		}
	}

	/// The working tree, or an error if this repository was opened without a work-dir.
	fn worktree(&self) -> Result<&WorkTree<WorktreeFileStore, DescriptorWorkDir, H>, RepoError> {
		match self {
			Held::Worktree(wt) => Ok(wt),
			Held::Plumbing(_) => Err(RepoError::Invalid(
				"no working tree; open the repository with open-worktree to grant a work-dir".to_owned(),
			)),
		}
	}
}

/// Run `body` once with `held` bound to whichever concrete [`Held<H>`] this value
/// holds — the two-arm runtime→compile-time bridge, written once. Plumbing bodies use
/// `held.repository()`; working-tree bodies use `held.worktree()?`.
macro_rules! dispatch {
	($self:expr, $held:ident => $body:expr) => {
		match $self {
			Self::Sha1($held) => $body,
			Self::Sha256($held) => $body,
		}
	};
}

/// The repository under its runtime-detected hash algorithm. The same
/// runtime→compile-time bridge as `gta-core`'s dispatch: match once here, and each
/// operation body is written once, generic over `H` (see [`crate::ops`]).
pub(crate) enum Inner {
	Sha1(Held<Sha1>),
	Sha256(Held<Sha256>),
}

impl Inner {
	/// Open the repository whose git dir *is* its common dir (an ordinary,
	/// non-linked repository) over the single granted descriptor — plumbing only, no
	/// working tree.
	pub(crate) fn open(git_dir: Descriptor) -> Result<Self, RepoError> {
		let store = WorktreeFileStore::single(LocalFileStore::from_descriptor(git_dir));
		Self::plumbing(store)
	}

	/// Open a repository together with its working tree, routing per-worktree paths to
	/// `git_dir`, shared paths (objects, refs, `packed-refs`, `config`) to `common_dir`,
	/// and working-tree access to `work_dir`. For an ordinary repository `git_dir` and
	/// `common_dir` are the same descriptor; for a linked worktree they differ.
	pub(crate) fn open_worktree(
		git_dir: Descriptor,
		common_dir: Descriptor,
		work_dir: Descriptor,
	) -> Result<Self, RepoError> {
		let store = WorktreeFileStore::from_stores(
			LocalFileStore::from_descriptor(common_dir),
			LocalFileStore::from_descriptor(git_dir),
		);
		let work = DescriptorWorkDir::from_descriptor(work_dir);
		// The `git_dir` path a `WorkTree` carries is inert here — the crate routes the index and all
		// git-dir files through the `FileStore`, so a placeholder suffices; the two match arms are
		// mutually exclusive, so moving `store`/`work` in each is fine.
		Ok(match Self::detect(&store)? {
			HashKind::Sha1 => Self::Sha1(Held::Worktree(WorkTree::new(
				Repository::new(ObjectStore::new(store)),
				work,
				"",
			))),
			HashKind::Sha256 => Self::Sha256(Held::Worktree(WorkTree::new(
				Repository::new(ObjectStore::new(store)),
				work,
				"",
			))),
		})
	}

	/// Detect the object format from `config` *through the store* (which reads it from
	/// the common dir) and open a plumbing-only repository as the matching `H`.
	fn plumbing(store: WorktreeFileStore) -> Result<Self, RepoError> {
		Ok(match Self::detect(&store)? {
			HashKind::Sha1 => Self::Sha1(Held::Plumbing(Repository::new(ObjectStore::new(store)))),
			HashKind::Sha256 => Self::Sha256(Held::Plumbing(Repository::new(ObjectStore::new(store)))),
		})
	}

	/// Read the repository's object format from its `config`.
	fn detect(store: &WorktreeFileStore) -> Result<HashKind, RepoError> {
		match block_on(detect_hash_kind(store)).map_err(ops::repo_error)? {
			gitana_object::HashKind::Sha1 => Ok(HashKind::Sha1),
			gitana_object::HashKind::Sha256 => Ok(HashKind::Sha256),
		}
	}

	/// Lay out git's directory skeleton, write the fresh-repo metadata for the
	/// *requested* algorithm (idempotent), and open — refusing a directory that
	/// already holds a repository of a different format. A fresh repository has no
	/// checked-out working tree, so `init` opens it plumbing-only.
	pub(crate) fn init(git_dir: Descriptor, kind: HashKind) -> Result<Self, RepoError> {
		let store = LocalFileStore::from_descriptor(git_dir);
		block_on(ops::init_layout(&store))?;
		let store = WorktreeFileStore::single(store);
		let inner = match kind {
			HashKind::Sha1 => Self::Sha1(Held::Plumbing(Repository::new(ObjectStore::new(store)))),
			HashKind::Sha256 => Self::Sha256(Held::Plumbing(Repository::new(ObjectStore::new(store)))),
		};
		dispatch!(&inner, held => block_on(ops::init_repo(held.repository())))?;
		Ok(inner)
	}

	/// Clone the Smart HTTP remote at `url` into the freshly-granted `git_dir`/`work_dir`
	/// descriptors. The object format is negotiated from the remote's advertisement (there is
	/// no local config to detect one from yet), then the git skeleton is laid and the clone runs
	/// under the matching `H`. Consumes both descriptors — a clone populates directories rather
	/// than opening a resource, so this returns unit; reopen with `open-worktree` to operate on
	/// the result.
	pub(crate) fn clone(
		git_dir: Descriptor,
		work_dir: Descriptor,
		url: &str,
	) -> Result<(), RepoError> {
		let origin = Origin::parse(url).map_err(|e| RepoError::Invalid(e.to_string()))?;
		let (advertisement, kind) = block_on(ops::clone_negotiate(&origin))?;

		let git = LocalFileStore::from_descriptor(git_dir);
		block_on(ops::init_layout(&git))?;
		let store = WorktreeFileStore::single(git);
		let work = DescriptorWorkDir::from_descriptor(work_dir);

		match kind {
			HashKind::Sha1 => block_on(ops::clone::<Sha1>(store, work, &origin, &advertisement)),
			HashKind::Sha256 => block_on(ops::clone::<Sha256>(store, work, &origin, &advertisement)),
		}
	}

	pub(crate) fn hash_kind(&self) -> HashKind {
		match self {
			Self::Sha1(_) => HashKind::Sha1,
			Self::Sha256(_) => HashKind::Sha256,
		}
	}

	pub(crate) fn read_config(&self) -> Result<String, RepoError> {
		dispatch!(self, held => block_on(ops::read_config(held.repository())))
	}

	pub(crate) fn read_object(&self, spec: &str) -> Result<ObjectInfo, RepoError> {
		dispatch!(self, held => block_on(ops::read_object(held.repository(), spec)))
	}

	pub(crate) fn read_blob(&self, spec: &str) -> Result<Vec<u8>, RepoError> {
		dispatch!(self, held => block_on(ops::read_blob(held.repository(), spec)))
	}

	pub(crate) fn read_commit(&self, spec: &str) -> Result<CommitInfo, RepoError> {
		dispatch!(self, held => block_on(ops::read_commit(held.repository(), spec)))
	}

	pub(crate) fn read_tag(&self, spec: &str) -> Result<TagInfo, RepoError> {
		dispatch!(self, held => block_on(ops::read_tag(held.repository(), spec)))
	}

	pub(crate) fn ls_tree(&self, spec: &str) -> Result<Vec<TreeEntry>, RepoError> {
		dispatch!(self, held => block_on(ops::ls_tree(held.repository(), spec)))
	}

	pub(crate) fn rev_parse(&self, spec: &str) -> Result<String, RepoError> {
		dispatch!(self, held => block_on(ops::rev_parse(held.repository(), spec)))
	}

	pub(crate) fn rev_list(
		&self,
		tips: &[String],
		max_count: Option<u32>,
	) -> Result<Vec<String>, RepoError> {
		dispatch!(self, held => block_on(ops::rev_list(held.repository(), tips, max_count)))
	}

	pub(crate) fn merge_base(&self, commits: &[String]) -> Result<Vec<String>, RepoError> {
		dispatch!(self, held => block_on(ops::merge_base(held.repository(), commits)))
	}

	pub(crate) fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, RepoError> {
		dispatch!(self, held => block_on(ops::is_ancestor(held.repository(), ancestor, descendant)))
	}

	pub(crate) fn list_refs(&self, prefix: &str) -> Result<Vec<RefEntry>, RepoError> {
		dispatch!(self, held => block_on(ops::list_refs(held.repository(), prefix)))
	}

	pub(crate) fn head(&self) -> Result<HeadState, RepoError> {
		dispatch!(self, held => block_on(ops::head(held.repository())))
	}

	pub(crate) fn resolve_ref(&self, name: &str) -> Result<Option<String>, RepoError> {
		dispatch!(self, held => block_on(ops::resolve_ref(held.repository(), name)))
	}

	pub(crate) fn update_ref(
		&self,
		name: &str,
		new: &str,
		expected: Option<&str>,
		reflog: Option<&ReflogRequest>,
	) -> Result<(), RepoError> {
		dispatch!(self, held => block_on(ops::update_ref(held.repository(), name, new, expected, reflog)))
	}

	pub(crate) fn delete_ref(&self, name: &str, expected: &str) -> Result<(), RepoError> {
		dispatch!(self, held => block_on(ops::delete_ref(held.repository(), name, expected)))
	}

	pub(crate) fn read_symbolic_ref(&self, name: &str) -> Result<Option<String>, RepoError> {
		dispatch!(self, held => block_on(ops::read_symbolic_ref(held.repository(), name)))
	}

	pub(crate) fn set_symbolic_ref(
		&self,
		name: &str,
		target: &str,
		reflog: Option<&ReflogRequest>,
	) -> Result<(), RepoError> {
		dispatch!(self, held => block_on(ops::set_symbolic_ref(held.repository(), name, target, reflog)))
	}

	pub(crate) fn write_blob(&self, data: &[u8]) -> Result<String, RepoError> {
		dispatch!(self, held => block_on(ops::write_blob(held.repository(), data)))
	}

	pub(crate) fn repack(&self, geometric: bool) -> Result<Option<RepackReport>, RepoError> {
		dispatch!(self, held => block_on(ops::repack(held.repository(), geometric)))
	}

	pub(crate) fn fetch(&self, url: &str) -> Result<FetchOutcome, RepoError> {
		dispatch!(self, held => block_on(ops::fetch(held.repository(), url)))
	}

	pub(crate) fn push(
		&self,
		url: &str,
		force: bool,
		delete: Option<String>,
	) -> Result<PushOutcome, RepoError> {
		dispatch!(self, held => block_on(ops::push(held.repository(), url, force, delete)))
	}

	pub(crate) fn write_tree(&self, entries: Vec<TreeBuildEntry>) -> Result<String, RepoError> {
		dispatch!(self, held => block_on(ops::write_tree(held.repository(), entries)))
	}

	pub(crate) fn create_commit(
		&self,
		tree: &str,
		parents: &[String],
		author: &str,
		committer: &str,
		message: &str,
	) -> Result<String, RepoError> {
		dispatch!(self, held => block_on(ops::create_commit(held.repository(), tree, parents, author, committer, message)))
	}

	pub(crate) fn status(&self) -> Result<WorktreeStatus, RepoError> {
		dispatch!(self, held => block_on(ops::status(held.worktree()?)))
	}

	pub(crate) fn add(&self, pathspecs: &[String], prefix: &str) -> Result<(), RepoError> {
		dispatch!(self, held => block_on(ops::add(held.worktree()?, pathspecs, prefix)))
	}

	pub(crate) fn checkout(&self, tree_ish: &str, force: bool) -> Result<(), RepoError> {
		dispatch!(self, held => block_on(ops::checkout(held.worktree()?, tree_ish, force)))
	}

	pub(crate) fn commit(
		&self,
		message: &str,
		author: &str,
		committer: &str,
	) -> Result<String, RepoError> {
		dispatch!(self, held => block_on(ops::commit(held.worktree()?, message, author, committer)))
	}
}
