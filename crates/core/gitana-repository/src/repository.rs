use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::{Commit, HashAlgorithm, ObjectId, ObjectKind, encode_commit, parse_commit};
use gitana_object_store::ObjectStore;

use crate::tree::{FlatEntry, build_tree};
use crate::{Config, HeadState, RefStore, RepositoryError, TreeBuildEntry};

/// A git repository: the object graph plus refs, over one repo-scoped store.
///
/// The engine is storage-agnostic and generic over the object-hash algorithm `H`. The
/// local profile points the file store at a `.git` directory (with a sentinel repo-id)
/// so the on-disk bytes are exactly what `git` expects for that object format. `init`
/// writes the metadata files (`config`, `HEAD`); creating the empty `objects/`/`refs/`
/// directory skeleton a real git repo needs is a filesystem concern handled by the
/// local wiring.
pub struct Repository<F, H: HashAlgorithm> {
	objects: ObjectStore<F, H>,
}

impl<F, H> Repository<F, H>
where
	F: FileStore,
	H: HashAlgorithm,
{
	/// Wrap a repo-scoped object store as a repository.
	pub fn new(objects: ObjectStore<F, H>) -> Self {
		Self { objects }
	}

	/// The object store (read/write objects, packs).
	pub fn objects(&self) -> &ObjectStore<F, H> {
		&self.objects
	}

	/// The ref store (HEAD, branches, tags).
	pub fn refs(&self) -> RefStore<'_, F, H> {
		RefStore::new(self.objects.file_store())
	}

	/// Write the metadata files for a fresh repo: a `config` matching the hash algorithm
	/// `H` and a symbolic `HEAD → refs/heads/main`. Idempotent — existing files are left
	/// untouched.
	pub async fn init(&self) -> Result<(), RepositoryError> {
		let files = self.objects.file_store();

		files
			.write_path_if_absent("config", Config::for_algorithm::<H>().render().as_bytes())
			.await?;
		let head = HeadState::<H>::Symbolic("refs/heads/main".to_owned()).render();
		files.write_path_if_absent("HEAD", head.as_bytes()).await?;
		Ok(())
	}

	/// Write a blob object, returning its id.
	pub async fn write_blob(&self, data: &[u8]) -> Result<ObjectId<H>, RepositoryError> {
		Ok(self.objects.write_object(ObjectKind::Blob, data).await?)
	}

	/// Build the nested tree objects for `entries` and return the root tree id.
	pub async fn write_tree(
		&self,
		entries: &[TreeBuildEntry<H>],
	) -> Result<ObjectId<H>, RepositoryError> {
		build_tree(&self.objects, entries).await
	}

	/// Recursively read a tree into `(path, mode, oid)` entries (`ls-tree -r`).
	pub async fn read_tree(&self, tree: ObjectId<H>) -> Result<Vec<FlatEntry<H>>, RepositoryError> {
		crate::tree::read_tree_recursive(&self.objects, tree).await
	}

	/// Peel `id` — a commit, tag, or tree — to its tree id, dereferencing tags;
	/// errors on a blob.
	pub async fn peel_to_tree(&self, id: ObjectId<H>) -> Result<ObjectId<H>, RepositoryError> {
		crate::revision::peel_to_tree(self, id).await
	}

	/// Peel `id` to a commit id, dereferencing an (annotated) tag chain; errors on a non-commit.
	pub async fn peel_to_commit(&self, id: ObjectId<H>) -> Result<ObjectId<H>, RepositoryError> {
		crate::revision::peel_to_commit(self, id).await
	}

	/// Read and parse the full git `config` file.
	pub async fn read_config(&self) -> Result<gitana_config::GitConfig, RepositoryError> {
		let bytes = self.objects.file_store().read_path("config").await?;
		let text = std::str::from_utf8(&bytes)
			.map_err(|_| RepositoryError::UnsupportedFormat("config is not UTF-8".to_owned()))?;
		gitana_config::GitConfig::parse(text)
			.map_err(|error| RepositoryError::UnsupportedFormat(error.to_string()))
	}

	/// The maximum pack size `repack` should target (`pack.packSizeLimit`), clamped to
	/// `[1 MiB, MAX_PACK_SIZE]`. Unset or non-positive falls back to `MAX_PACK_SIZE`, i.e. a single
	/// pack. Blob-store or memory-constrained deployments set a lower value to split into several
	/// packs.
	pub async fn pack_size_limit(&self) -> Result<u64, RepositoryError> {
		const MIN_PACK_SIZE: u64 = 1 << 20;
		let config = self.read_config().await?;
		let configured = config
			.get_int("pack", None, "packsizelimit")
			.map_err(|error| {
				RepositoryError::UnsupportedFormat(format!("pack.packSizeLimit: {error}"))
			})?;
		let limit = match configured {
			Some(value) if value > 0 => value as u64,
			_ => gitana_object_store::MAX_PACK_SIZE,
		};
		Ok(limit.clamp(MIN_PACK_SIZE, gitana_object_store::MAX_PACK_SIZE))
	}

	/// Write `config` to the `config` file, replacing its contents (last-writer-wins, retrying on
	/// a concurrent change).
	pub async fn write_config(
		&self,
		config: &gitana_config::GitConfig,
	) -> Result<(), RepositoryError> {
		let bytes = config.render().into_bytes();
		let store = self.objects.file_store();
		loop {
			let expected = match store.read_path_versioned("config").await {
				Ok((_, version)) => Some(version),
				Err(FileStoreError::NotFound) => None,
				Err(error) => return Err(error.into()),
			};
			match store
				.write_path_cas("config", &bytes, expected.as_ref())
				.await
			{
				Ok(_) => return Ok(()),
				Err(FileStoreError::VersionMismatch) => continue,
				Err(error) => return Err(error.into()),
			}
		}
	}

	/// Read a blob's content.
	pub async fn read_blob(&self, id: ObjectId<H>) -> Result<Vec<u8>, RepositoryError> {
		let (kind, payload) = self.objects.read_object(&id).await?;
		if kind != ObjectKind::Blob {
			return Err(RepositoryError::InvalidRef(format!("{id} is not a blob")));
		}
		Ok(payload)
	}

	/// Read a commit and return the tree it points at.
	pub async fn commit_tree(&self, commit: ObjectId<H>) -> Result<ObjectId<H>, RepositoryError> {
		let (kind, payload) = self.objects.read_object(&commit).await?;
		if kind != ObjectKind::Commit {
			return Err(RepositoryError::InvalidRef(format!(
				"{commit} is not a commit"
			)));
		}
		Ok(parse_commit::<H>(&payload)?.tree)
	}

	/// Write a commit object (no ref update), returning its id. `author` and
	/// `committer` are git identity lines (`Name <email> seconds ±hhmm`).
	pub async fn create_commit(
		&self,
		tree: ObjectId<H>,
		parents: Vec<ObjectId<H>>,
		author: &str,
		committer: &str,
		message: &str,
	) -> Result<ObjectId<H>, RepositoryError> {
		let commit = Commit {
			tree,
			parents,
			author: author.to_owned(),
			committer: committer.to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: message.to_owned(),
		};
		Ok(
			self
				.objects
				.write_object(ObjectKind::Commit, &encode_commit(&commit))
				.await?,
		)
	}

	/// The branch `HEAD` points at and its current tip (`None` on an unborn branch) — the starting
	/// point for recording a commit on `HEAD`. Errors on a detached `HEAD` (not yet supported), so a
	/// caller need not repeat that guard. Used by [`Self::commit_on_head`] and by the signed-commit
	/// path, which must resolve the parent to build the object before its id is known.
	pub async fn head_branch_tip(&self) -> Result<(String, Option<ObjectId<H>>), RepositoryError> {
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
		Ok((target, parent))
	}

	/// Record an already-written `commit` (built on `parent`, from [`Self::head_branch_tip`]) as the
	/// new tip of branch `target`: advance it via CAS and append `commit`/`commit (initial)` reflog
	/// entries to the branch and `HEAD`. The ref half of [`Self::commit_on_head`], split out so the
	/// signed-commit path — which builds and signs the object first — reuses the identical logic.
	pub async fn record_commit(
		&self,
		target: &str,
		parent: Option<ObjectId<H>>,
		commit: ObjectId<H>,
		committer: &str,
		message: &str,
	) -> Result<(), RepositoryError> {
		let refs = self.refs();
		refs.update_ref(target, commit, parent).await?;

		let subject = message.lines().next().unwrap_or("");
		let reflog = if parent.is_none() {
			format!("commit (initial): {subject}")
		} else {
			format!("commit: {subject}")
		};
		refs
			.append_reflog(target, parent, commit, committer, &reflog)
			.await?;
		refs
			.append_reflog("HEAD", parent, commit, committer, &reflog)
			.await?;
		Ok(())
	}

	/// Create a commit on the branch `HEAD` points at, advancing the branch via CAS
	/// and appending reflog entries to the branch and `HEAD`. Returns the commit id.
	/// Detached HEAD is not yet supported.
	pub async fn commit_on_head(
		&self,
		tree: ObjectId<H>,
		author: &str,
		committer: &str,
		message: &str,
	) -> Result<ObjectId<H>, RepositoryError> {
		let (target, parent) = self.head_branch_tip().await?;
		let parents = parent.map(|p| vec![p]).unwrap_or_default();
		let commit = self
			.create_commit(tree, parents, author, committer, message)
			.await?;
		self
			.record_commit(&target, parent, commit, committer, message)
			.await?;
		Ok(commit)
	}

	/// Record an already-written two-parent merge `commit` (built on `parent`, from
	/// [`Self::head_branch_tip`]) as the new tip of branch `target`: advance it via CAS and append
	/// `commit (merge):` reflog entries to the branch and `HEAD`. Like [`Self::record_commit`] but for
	/// concluding a merge — the porcelain builds the (optionally signed) merge commit, then calls this.
	pub async fn record_merge_commit(
		&self,
		target: &str,
		parent: ObjectId<H>,
		commit: ObjectId<H>,
		committer: &str,
		message: &str,
	) -> Result<(), RepositoryError> {
		let refs = self.refs();
		refs.update_ref(target, commit, Some(parent)).await?;
		let subject = message.lines().next().unwrap_or("");
		let reflog = format!("commit (merge): {subject}");
		refs
			.append_reflog(target, Some(parent), commit, committer, &reflog)
			.await?;
		refs
			.append_reflog("HEAD", Some(parent), commit, committer, &reflog)
			.await?;
		Ok(())
	}

	/// Move the current branch (or detached `HEAD`) to `commit` via CAS, recording the previous
	/// tip in `ORIG_HEAD` and appending a reflog entry (`message`, e.g. `reset: moving to
	/// HEAD~1`) to the branch and `HEAD`. The index and working tree are not touched. Mirrors the
	/// ref half of [`Self::commit_on_head`], but for a reset rather than a new commit.
	pub async fn reset_head(
		&self,
		commit: ObjectId<H>,
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

	/// Record `commit` as `ORIG_HEAD` so it can be recovered (`gta reset ORIG_HEAD`), as git does
	/// when starting an operation that may move or replace `HEAD`.
	pub async fn set_orig_head(&self, commit: ObjectId<H>) -> Result<(), RepositoryError> {
		let refs = self.refs();
		let current = refs.resolve("ORIG_HEAD").await?;
		refs.update_ref("ORIG_HEAD", commit, current).await?;
		Ok(())
	}

	/// The commit recorded in `ORIG_HEAD`, or `None` if it is unset.
	pub async fn orig_head(&self) -> Result<Option<ObjectId<H>>, RepositoryError> {
		self.refs().resolve("ORIG_HEAD").await
	}

	/// Record an in-progress merge: `MERGE_HEAD` (the commit being merged) and `MERGE_MSG` (the
	/// prepared commit message).
	pub async fn start_merge(
		&self,
		merge_head: ObjectId<H>,
		message: &str,
	) -> Result<(), RepositoryError> {
		crate::merge_state::start_merge(self, merge_head, message).await
	}

	/// The commit recorded in `MERGE_HEAD`, or `None` when no merge is in progress.
	pub async fn merge_head(&self) -> Result<Option<ObjectId<H>>, RepositoryError> {
		crate::merge_state::merge_head(self).await
	}

	/// The prepared merge message (`MERGE_MSG`), or `None`.
	pub async fn merge_msg(&self) -> Result<Option<String>, RepositoryError> {
		crate::merge_state::merge_msg(self).await
	}

	/// Clear the in-progress merge state (`MERGE_HEAD`, `MERGE_MSG`).
	pub async fn clear_merge(&self) -> Result<(), RepositoryError> {
		crate::merge_state::clear_merge(self).await
	}

	/// Record an in-progress cherry-pick: `CHERRY_PICK_HEAD` (the commit being picked) and `MERGE_MSG`.
	pub async fn start_cherry_pick(
		&self,
		commit: ObjectId<H>,
		message: &str,
	) -> Result<(), RepositoryError> {
		crate::merge_state::start_cherry_pick(self, commit, message).await
	}

	/// The commit recorded in `CHERRY_PICK_HEAD`, or `None` when no cherry-pick is in progress.
	pub async fn cherry_pick_head(&self) -> Result<Option<ObjectId<H>>, RepositoryError> {
		crate::merge_state::cherry_pick_head(self).await
	}

	/// Clear the in-progress cherry-pick state (`CHERRY_PICK_HEAD`, `MERGE_MSG`).
	pub async fn clear_cherry_pick(&self) -> Result<(), RepositoryError> {
		crate::merge_state::clear_cherry_pick(self).await
	}

	/// Record an in-progress revert: `REVERT_HEAD` (the commit being reverted) and `MERGE_MSG`.
	pub async fn start_revert(
		&self,
		commit: ObjectId<H>,
		message: &str,
	) -> Result<(), RepositoryError> {
		crate::merge_state::start_revert(self, commit, message).await
	}

	/// The commit recorded in `REVERT_HEAD`, or `None` when no revert is in progress.
	pub async fn revert_head(&self) -> Result<Option<ObjectId<H>>, RepositoryError> {
		crate::merge_state::revert_head(self).await
	}

	/// Clear the in-progress revert state (`REVERT_HEAD`, `MERGE_MSG`).
	pub async fn clear_revert(&self) -> Result<(), RepositoryError> {
		crate::merge_state::clear_revert(self).await
	}

	/// Record the start of a rebase (the `rebase-merge/` state directory).
	pub async fn start_rebase(&self, state: &crate::RebaseState<H>) -> Result<(), RepositoryError> {
		crate::rebase_state::start_rebase(self, state).await
	}

	/// The in-progress rebase state, or `None` when no rebase is underway.
	pub async fn rebase_state(&self) -> Result<Option<crate::RebaseState<H>>, RepositoryError> {
		crate::rebase_state::rebase_state(self).await
	}

	/// Whether a rebase is in progress.
	pub async fn rebase_in_progress(&self) -> Result<bool, RepositoryError> {
		crate::rebase_state::rebase_in_progress(self).await
	}

	/// Replace the rebase's remaining-commit list (oldest-first; current step first).
	pub async fn set_rebase_todo(&self, todo: &[ObjectId<H>]) -> Result<(), RepositoryError> {
		crate::rebase_state::set_rebase_todo(self, todo).await
	}

	/// Clear the in-progress rebase state.
	pub async fn clear_rebase(&self) -> Result<(), RepositoryError> {
		crate::rebase_state::clear_rebase(self).await
	}

	/// The commit ids at this repository's shallow boundary (`.git/shallow`) — commits whose parents
	/// are deliberately absent. Empty for a complete (non-shallow) repository.
	pub async fn read_shallow(&self) -> Result<Vec<ObjectId<H>>, RepositoryError> {
		crate::shallow::read_shallow(self).await
	}

	/// Replace the shallow boundary (`.git/shallow`) with `oids`; an empty `oids` deletes the file,
	/// making the repository complete again.
	pub async fn write_shallow(&self, oids: &[ObjectId<H>]) -> Result<(), RepositoryError> {
		crate::shallow::write_shallow(self, oids).await
	}

	/// Resolve a revision spec (`HEAD`, `main`, `<oid>`, `HEAD~2`, `v1^{commit}`, …)
	/// to an object id.
	pub async fn rev_parse(&self, spec: &str) -> Result<ObjectId<H>, RepositoryError> {
		crate::revision::rev_parse(self, spec).await
	}

	/// Walk commits reachable from `tips` in committer-date order (newest first).
	pub async fn rev_list(&self, tips: &[ObjectId<H>]) -> Result<Vec<ObjectId<H>>, RepositoryError> {
		crate::revision::rev_list(self, tips).await
	}

	/// The best common ancestor(s) — merge bases — of `commits`; empty if they share no ancestor.
	pub async fn merge_base(
		&self,
		commits: &[ObjectId<H>],
	) -> Result<Vec<ObjectId<H>>, RepositoryError> {
		crate::merge_base::merge_base(self, commits).await
	}

	/// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
	pub async fn is_ancestor(
		&self,
		ancestor: ObjectId<H>,
		descendant: ObjectId<H>,
	) -> Result<bool, RepositoryError> {
		crate::merge_base::is_ancestor(self, ancestor, descendant).await
	}

	/// Three-way merge the `ours` and `theirs` trees against their common `base` tree, returning the
	/// merged tree and the conflicted paths.
	pub async fn merge_trees(
		&self,
		base: ObjectId<H>,
		ours: ObjectId<H>,
		theirs: ObjectId<H>,
	) -> Result<crate::TreeMerge<H>, RepositoryError> {
		crate::merge::merge_trees(self, base, ours, theirs).await
	}

	/// Read and validate the repository config, requiring its object format to match the
	/// hash algorithm `H` this repository was opened as. Refuses unknown formats and a
	/// format/`H` mismatch (e.g. opening a sha1 repo as `Repository<_, Sha256>`).
	pub async fn open(&self) -> Result<Config, RepositoryError> {
		let config = Config::read(self.objects.file_store()).await?;
		if config.object_format != H::NAME {
			return Err(RepositoryError::UnsupportedFormat(format!(
				"repository is {}, opened as {}",
				config.object_format,
				H::NAME
			)));
		}
		Ok(config)
	}
}
