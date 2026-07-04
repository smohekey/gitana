use std::path::{Path, PathBuf};

use gitana_file_store::{FileStore, FileStoreError, WriteOutcome};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

use crate::fsmeta::{file_mode, mode_of, path_bytes, stat_of};
use crate::ignore::{self, DirIgnore};
use crate::{Index, IndexEntry, Status, WorktreeError};

/// A working directory paired with its repository.
///
/// Filesystem-coupled by nature: working-tree files are real files, so it reads/writes them
/// with `std::fs`, while blob objects and the index both go through the repository's file store
/// (so the index lives under the same capability as the rest of the git directory). The index is
/// written with git's lock-then-replace protocol, re-expressed on the store's compare-and-set
/// primitives. `add`/`status`/`checkout` build on this.
pub struct WorkTree<F, H: HashAlgorithm> {
	repo: Repository<F, H>,
	work_dir: PathBuf,
	git_dir: PathBuf,
}

/// Proof that the index lock (`index.lock`) is held: minted only by [`WorkTree::lock_index`] and
/// consumed by [`WorkTree::commit_index`] / [`WorkTree::release_index_lock`], so the index cannot be
/// written without first taking the lock. Dropping it without releasing leaves the lock in place (a
/// stale `index.lock`), exactly as dropping the open lock file did before.
pub(crate) struct IndexLock(());

impl<F: FileStore, H: HashAlgorithm> WorkTree<F, H> {
	/// Build a working tree over `repo`, with the working directory at `work_dir`
	/// and the git directory at `git_dir`.
	pub fn new(
		repo: Repository<F, H>,
		work_dir: impl Into<PathBuf>,
		git_dir: impl Into<PathBuf>,
	) -> Self {
		Self {
			repo,
			work_dir: work_dir.into(),
			git_dir: git_dir.into(),
		}
	}

	/// The underlying repository.
	pub fn repository(&self) -> &Repository<F, H> {
		&self.repo
	}

	/// This checkout's git directory: where its per-worktree files (`HEAD`, `index`) live. For a
	/// linked worktree this is `<main>/.git/worktrees/<name>`, not the shared common dir.
	pub fn git_dir(&self) -> &Path {
		&self.git_dir
	}

	/// Resolve a revision spec, including the index-relative forms the repository resolver cannot:
	/// `:<path>` (the staged blob, stage 0) and `:<n>:<path>` (merge stage `n`). Every other spec
	/// — refs, oids, `HEAD`, `~`/`^`/`^{type}`, and `<rev>:<path>` — is delegated to
	/// [`Repository::rev_parse`]. The path is repository-root-relative.
	pub async fn rev_parse(&self, spec: &str) -> Result<ObjectId<H>, WorktreeError> {
		match spec.strip_prefix(':') {
			Some(rest) => self.resolve_index_spec(rest).await,
			None => Ok(self.repository().rev_parse(spec).await?),
		}
	}

	/// Resolve the part of an index spec after the leading `:` to the staged blob's id.
	async fn resolve_index_spec(&self, rest: &str) -> Result<ObjectId<H>, WorktreeError> {
		// `:/text` (commit-message search) is not an index lookup.
		if rest.starts_with('/') {
			return Err(WorktreeError::InvalidIndexSpec(rest.to_owned()));
		}
		// `:<n>:<path>` selects merge stage `n` (0–3); otherwise stage 0.
		let bytes = rest.as_bytes();
		let (stage, path) = match bytes {
			[digit, b':', ..] if digit.is_ascii_digit() => {
				let stage = digit - b'0';
				if stage > 3 {
					return Err(WorktreeError::InvalidIndexSpec(rest.to_owned()));
				}
				(stage, &rest[2..])
			}
			_ => (0, rest),
		};
		self
			.load_index()
			.await?
			.entries
			.iter()
			.find(|entry| entry.path == path && entry.stage == stage)
			.map(|entry| entry.oid)
			.ok_or_else(|| {
				let at = if stage == 0 {
					String::new()
				} else {
					format!(" at stage {stage}")
				};
				WorktreeError::IndexPathMissing(path.to_owned(), at)
			})
	}

	pub(crate) fn work_dir(&self) -> &Path {
		&self.work_dir
	}

	/// The repository's file store — the capability the index (and `index.lock`) live under, the
	/// same one the object database and refs use. `index`/`index.lock` are per-worktree paths, so a
	/// linked worktree's store routes them to its own git directory.
	fn files(&self) -> &F {
		self.repo.objects().file_store()
	}

	/// Read and parse the index (`index`), or an empty index if it does not exist.
	pub async fn load_index(&self) -> Result<Index<H>, WorktreeError> {
		match self.files().read_path("index").await {
			Ok(bytes) => Ok(Index::parse(&bytes)?),
			Err(FileStoreError::NotFound) => Ok(Index::new()),
			Err(error) => Err(error.into()),
		}
	}

	/// Write the index under the lock (acquire, write, release).
	pub async fn save_index(&self, index: &Index<H>) -> Result<(), WorktreeError> {
		let lock = self.lock_index().await?;
		self.commit_index(lock, index).await
	}

	/// Acquire the index lock (`index.lock`), returning a guard proving it is held. Fails with
	/// [`WorktreeError::IndexLocked`] if another writer already holds it. Pair with
	/// [`Self::commit_index`] to write the index and release, or [`Self::release_index_lock`] to
	/// release without writing. Taking the lock up front lets a destructive operation fail before it
	/// mutates the working tree, rather than after.
	pub(crate) async fn lock_index(&self) -> Result<IndexLock, WorktreeError> {
		match self.files().write_path_if_absent("index.lock", &[]).await? {
			WriteOutcome::Written => Ok(IndexLock(())),
			WriteOutcome::AlreadyExists => Err(WorktreeError::IndexLocked),
		}
	}

	/// Write `index` while holding `lock`, then release the lock. The index is replaced atomically
	/// (`write_path_replace` writes to a temporary and renames), so a reader never sees a torn
	/// index. `write_path_replace` deliberately takes no lock file of its own — the git-level
	/// `index.lock` we already hold is the exclusion — so it does not contend for that same name the
	/// way a compare-and-set write would. The lock is released even if the write fails, so a failure
	/// does not strand `index.lock`.
	pub(crate) async fn commit_index(
		&self,
		lock: IndexLock,
		index: &Index<H>,
	) -> Result<(), WorktreeError> {
		let outcome = self
			.files()
			.write_path_replace("index", &index.write_v4())
			.await;
		self.release_index_lock(lock).await;
		Ok(outcome?)
	}

	/// Release a held index lock (from [`Self::lock_index`]) without writing, removing `index.lock`.
	/// Use when an operation fails after locking but before [`Self::commit_index`], so it does not
	/// leave a stale lock behind.
	pub(crate) async fn release_index_lock(&self, lock: IndexLock) {
		let _ = self.files().delete_path("index.lock", None).await;
		drop(lock);
	}

	/// Stage `pathspecs`, interpreted relative to `prefix` (a `/`-joined work-tree-relative
	/// subdirectory, empty at the root). A file is staged directly; a directory (or `.`) is
	/// walked, applying `.gitignore`, and its non-ignored files are staged; a path that no
	/// longer exists is removed from the index (a staged deletion).
	pub async fn add(&self, pathspecs: &[&str], prefix: &str) -> Result<(), WorktreeError> {
		let mut index = self.load_index().await?;
		let mut ignore_stack: Vec<DirIgnore> = Vec::new();
		for &spec in pathspecs {
			let (rel, dir_only) = crate::pathspec::normalize(spec, prefix)?;
			let full = if rel.is_empty() {
				self.work_dir.clone()
			} else {
				self.work_dir.join(&rel)
			};
			match std::fs::symlink_metadata(&full) {
				Ok(meta) if meta.is_dir() && !meta.is_symlink() => {
					let mut files = Vec::new();
					walk_files(&full, &rel, &mut ignore_stack, &mut files)?;
					for file in files {
						self.stage_file(&mut index, &file).await?;
					}
				}
				// A trailing-slash spec required a directory but resolved to a file or nothing.
				Ok(_) if dir_only => return Err(WorktreeError::PathspecMatch(spec.to_owned())),
				Ok(_) => self.stage_file(&mut index, &rel).await?,
				Err(error) if error.kind() == std::io::ErrorKind::NotFound && dir_only => {
					return Err(WorktreeError::PathspecMatch(spec.to_owned()));
				}
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => index.remove(&rel),
				Err(error) => return Err(error.into()),
			}
		}
		self.save_index(&index).await
	}

	async fn stage_file(&self, index: &mut Index<H>, path: &str) -> Result<(), WorktreeError> {
		let full = self.work_dir.join(path);
		match std::fs::symlink_metadata(&full) {
			Ok(meta) if meta.is_symlink() => {
				let target = std::fs::read_link(&full)?;
				let oid = self.repo.write_blob(path_bytes(&target)).await?;
				index.remove_type_conflicts(path);
				index.upsert(entry(path, 0o120000, oid, &meta));
			}
			Ok(meta) if meta.is_file() => {
				let content = std::fs::read(&full)?;
				let oid = self.repo.write_blob(&content).await?;
				index.remove_type_conflicts(path);
				index.upsert(entry(path, file_mode(&meta), oid, &meta));
			}
			Ok(_) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => index.remove(path),
			Err(error) => return Err(error.into()),
		}
		Ok(())
	}

	/// Compute the three-way status: HEAD tree vs index (staged) and index vs
	/// working tree (unstaged), plus untracked files.
	pub async fn status(&self) -> Result<Status, WorktreeError> {
		crate::status::compute(self).await
	}

	/// Content changes between the index and the working tree (`git diff`).
	pub async fn diff_unstaged(&self) -> Result<Vec<crate::FileDiff>, WorktreeError> {
		crate::diff::unstaged(self).await
	}

	/// Content changes between the `HEAD` tree and the index (`git diff --cached`).
	pub async fn diff_staged(&self) -> Result<Vec<crate::FileDiff>, WorktreeError> {
		crate::diff::staged(self).await
	}

	/// Materialise `tree` into the working directory and index. Without `force`,
	/// refuses to overwrite uncommitted local changes. Does not move `HEAD`.
	pub async fn checkout(&self, tree: ObjectId<H>, force: bool) -> Result<(), WorktreeError> {
		crate::checkout::run(self, tree, force).await
	}

	/// Apply only the `from_tree` → `to_tree` diff (git's `read-tree -m -u` two-way merge, for a
	/// fast-forward): touch just the changed paths, leaving unrelated staged or dirty entries alone.
	/// Returns the changed paths whose local state would be overwritten (empty = applied; nothing is
	/// applied when non-empty). Does not move `HEAD`.
	pub async fn twoway_merge(
		&self,
		from_tree: ObjectId<H>,
		to_tree: ObjectId<H>,
	) -> Result<Vec<String>, WorktreeError> {
		crate::checkout::twoway_merge(self, from_tree, to_tree).await
	}

	/// Restore `pathspecs` from `source` (a tree; `None` = the current index) into the chosen
	/// targets — the working tree (`worktree`) and/or the index (`staged`) — discarding any
	/// uncommitted changes to those paths. A selected path absent from the source but currently
	/// tracked is removed from the chosen targets. Does not move `HEAD`. `pathspecs` are
	/// interpreted relative to `prefix` (a `/`-joined work-tree-relative subdirectory, empty
	/// at the root).
	pub async fn restore(
		&self,
		source: Option<ObjectId<H>>,
		worktree: bool,
		staged: bool,
		pathspecs: &[&str],
		prefix: &str,
	) -> Result<(), WorktreeError> {
		crate::restore::run(self, source, worktree, staged, pathspecs, prefix, true).await
	}

	/// Reset the index to `tree`, replacing every entry with the tree's content (the index half
	/// of `git reset --mixed`). The working tree is left untouched, and `HEAD` is not moved.
	pub async fn reset_index(&self, tree: ObjectId<H>) -> Result<(), WorktreeError> {
		crate::reset::run(self, tree).await
	}

	/// Reset the index entries matched by `pathspecs` to their state in `tree` (the index half of
	/// `git reset [<commit>] -- <paths>`): matched entries present in `tree` are restaged from it,
	/// matched entries absent from it are unstaged. The working tree and `HEAD` are untouched.
	/// Unlike [`Self::restore`], a pathspec that matches nothing is a no-op, not an error, so
	/// `reset -- <untracked-or-missing>` succeeds as in git. `pathspecs` are relative to `prefix`.
	pub async fn reset_index_paths(
		&self,
		tree: ObjectId<H>,
		pathspecs: &[&str],
		prefix: &str,
	) -> Result<(), WorktreeError> {
		crate::restore::run(self, Some(tree), false, true, pathspecs, prefix, false).await
	}

	/// Remove the tracked paths matched by `pathspecs` from the index and — unless `cached` —
	/// from the working tree (`git rm`). Pathspecs match tracked paths only; a directory match
	/// needs `recursive`. Without `force`, git's data-safety check refuses to lose un-saved
	/// changes. With `dry_run`, nothing is written. Removal is per-path: see [`RmOutcome`] for the
	/// removed paths and any failure. `pathspecs` are interpreted relative to `prefix`.
	#[allow(clippy::too_many_arguments)]
	pub async fn rm(
		&self,
		pathspecs: &[&str],
		prefix: &str,
		cached: bool,
		force: bool,
		recursive: bool,
		dry_run: bool,
	) -> Result<crate::RmOutcome, WorktreeError> {
		crate::rm::run(self, pathspecs, prefix, cached, force, recursive, dry_run).await
	}

	/// Move/rename tracked `sources` to `dest` (`git mv`): a filesystem rename plus an index
	/// update. With one source and a `dest` that is not an existing directory, `dest` is the new
	/// path; otherwise each source moves into the directory `dest`. The destination must not
	/// exist unless `force`. With `dry_run`, nothing is moved. Returns the `(from, to)` pairs
	/// performed. `sources` and `dest` are interpreted relative to `prefix`.
	pub async fn mv(
		&self,
		sources: &[&str],
		dest: &str,
		prefix: &str,
		force: bool,
		dry_run: bool,
	) -> Result<Vec<(String, String)>, WorktreeError> {
		crate::mv::run(self, sources, dest, prefix, force, dry_run).await
	}
}

fn entry<H: HashAlgorithm>(
	path: &str,
	mode: u32,
	oid: ObjectId<H>,
	meta: &std::fs::Metadata,
) -> IndexEntry<H> {
	IndexEntry {
		stat: stat_of(meta),
		mode,
		oid,
		stage: 0,
		assume_valid: false,
		path: path.to_owned(),
	}
}

/// Whether a working-tree file matches an index entry by its stat cache and mode
/// (the fast path that avoids re-hashing).
pub(crate) fn stat_matches<H: HashAlgorithm>(
	entry: &IndexEntry<H>,
	meta: &std::fs::Metadata,
) -> bool {
	entry.mode == mode_of(meta) && entry.stat == stat_of(meta)
}

/// Collect all non-ignored files under `dir_path` (recursively), applying
/// `.gitignore` and skipping `.git`. Used to expand a directory pathspec for `add`.
fn walk_files(
	dir_path: &Path,
	dir_rel: &str,
	stack: &mut Vec<DirIgnore>,
	out: &mut Vec<String>,
) -> Result<(), WorktreeError> {
	let pushed = match std::fs::read_to_string(dir_path.join(".gitignore")) {
		Ok(text) => {
			stack.push(ignore::parse(&text, dir_rel));
			true
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
		Err(error) => return Err(error.into()),
	};

	for entry in std::fs::read_dir(dir_path)? {
		let entry = entry?;
		let name = entry.file_name();
		let name = name.to_string_lossy();
		if name == ".git" {
			continue;
		}
		let rel = if dir_rel.is_empty() {
			name.into_owned()
		} else {
			format!("{dir_rel}/{name}")
		};
		let is_dir = entry.metadata()?.is_dir();
		if ignore::is_ignored(&rel, is_dir, stack) {
			continue;
		}
		if is_dir {
			walk_files(&entry.path(), &rel, stack, out)?;
		} else {
			out.push(rel);
		}
	}

	if pushed {
		stack.pop();
	}
	Ok(())
}
