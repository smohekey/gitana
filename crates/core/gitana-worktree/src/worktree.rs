use std::path::{Path, PathBuf};

use gitana_file_store::{FileStore, FileStoreError, WriteOutcome};
use gitana_file_store_local::{Meta, WorkDirFs};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

use crate::fsmeta::{effective_mode, join_rel, mode_of, push_gitignore, stat_of};
use crate::ignore::{self, DirIgnore};
use crate::{Index, IndexEntry, Status, WorktreeError};

/// A working directory paired with its repository.
///
/// Filesystem-coupled by nature, but no longer through ambient authority: working-tree files are
/// read and written through a [`WorkDirFs`] capability (`work`), while blob objects and the index
/// go through the repository's file store (so the index lives under the same capability as the rest
/// of the git directory). The index is written with git's lock-then-replace protocol.
/// `add`/`status`/`checkout` build on this.
pub struct WorkTree<F, W, H: HashAlgorithm> {
	repo: Repository<F, H>,
	work: W,
	git_dir: PathBuf,
}

/// Proof that the index lock (`index.lock`) is held: minted only by [`WorkTree::lock_index`] and
/// consumed by [`WorkTree::commit_index`] / [`WorkTree::release_index_lock`], so the index cannot be
/// written without first taking the lock. Dropping it without releasing leaves the lock in place (a
/// stale `index.lock`), exactly as dropping the open lock file did before.
pub(crate) struct IndexLock(());

impl<F: FileStore, W: WorkDirFs, H: HashAlgorithm> WorkTree<F, W, H> {
	/// Build a working tree over `repo`, with the working directory served by the capability `work`
	/// and the git directory at `git_dir`.
	pub fn new(repo: Repository<F, H>, work: W, git_dir: impl Into<PathBuf>) -> Self {
		Self {
			repo,
			work,
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

	/// The working-tree filesystem capability (all working-tree paths are relative to its root).
	pub(crate) fn work(&self) -> &W {
		&self.work
	}

	/// The repository's file store — the capability the index (and `index.lock`) live under, the
	/// same one the object database and refs use. `index`/`index.lock` are per-worktree paths, so a
	/// linked worktree's store routes them to its own git directory.
	fn files(&self) -> &F {
		self.repo.objects().file_store()
	}

	/// Read and parse the index (`index`), or an empty index if it does not exist.
	pub async fn load_index(&self) -> Result<Index<H>, WorktreeError> {
		let bytes = match self.files().read_path("index").await {
			Ok(bytes) => bytes,
			Err(FileStoreError::NotFound) => return Ok(Index::new()),
			Err(error) => return Err(error.into()),
		};
		let (index, link) = Index::parse_with_link(&bytes)?;
		match link {
			// A split index: load the referenced shared (base) index and merge it in, so status/read see the
			// effective index git would (an absent shared file is corruption, not an empty index).
			Some(link) if !crate::index::is_null_oid(&link.shared_oid) => {
				let shared = format!("sharedindex.{}", link.shared_oid.to_hex());
				let shared_bytes = self
					.files()
					.read_path(&shared)
					.await
					.map_err(|error| match error {
						FileStoreError::NotFound => {
							WorktreeError::Malformed(format!("missing shared index {shared}"))
						}
						other => other.into(),
					})?;
				// Integrity: the shared index's trailing checksum must equal the link oid (git names the file by,
				// and verifies, that checksum). `Index::parse` only validates the file against its *own* trailer,
				// so without this a substituted/stale `sharedindex.*` could supply a different staging state and
				// make a modified checkout look clean. The file's own trailer is its verified content hash.
				let trailer = shared_bytes
					.get(shared_bytes.len().saturating_sub(H::RAW_LEN)..)
					.unwrap_or(&[]);
				if trailer != link.shared_oid.as_bytes() {
					return Err(WorktreeError::Malformed(format!(
						"shared index {shared} checksum does not match link oid"
					)));
				}
				let base = Index::parse(&shared_bytes)?;
				crate::index::merge_split_index(base, index, &link)
			}
			_ => Ok(index),
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
			// The empty spec (`.` at the work-tree root) always names the root directory to walk.
			if rel.is_empty() {
				let mut files = Vec::new();
				walk_files(self.work(), "", &mut ignore_stack, &mut files)?;
				for file in files {
					self.stage_file(&mut index, &file).await?;
				}
				self.stage_deletions(&mut index, "")?;
				continue;
			}
			match self.work().lstat(&rel)? {
				Some(meta) if meta.kind.is_dir() => {
					let mut files = Vec::new();
					walk_files(self.work(), &rel, &mut ignore_stack, &mut files)?;
					for file in files {
						self.stage_file(&mut index, &file).await?;
					}
					self.stage_deletions(&mut index, &rel)?;
				}
				// A trailing-slash spec required a directory but resolved to a file.
				Some(_) if dir_only => return Err(WorktreeError::PathspecMatch(spec.to_owned())),
				Some(_) => self.stage_file(&mut index, &rel).await?,
				// Absent from the working tree: stage the deletion of whatever tracked entries the
				// pathspec covers — the exact path (a removed file) and any children (a removed
				// directory, `rm -r dir && add dir`). A spec matching no tracked entry did not match
				// anything; a trailing-slash spec additionally requires it to have named a directory.
				None => {
					let child_prefix = format!("{rel}/");
					let matched = index.entry(&rel).is_some()
						|| index
							.entries
							.iter()
							.any(|entry| entry.stage == 0 && entry.path.starts_with(&child_prefix));
					if dir_only && !matched {
						return Err(WorktreeError::PathspecMatch(spec.to_owned()));
					}
					index.remove(&rel);
					self.stage_deletions(&mut index, &rel)?;
				}
			}
		}
		self.save_index(&index).await
	}

	async fn stage_file(&self, index: &mut Index<H>, path: &str) -> Result<(), WorktreeError> {
		match self.work().lstat(path)? {
			Some(meta) if meta.kind.is_symlink() => {
				let target = self.work().read_link(path)?;
				let oid = self.repo.write_blob(&target).await?;
				index.remove_type_conflicts(path);
				index.upsert(entry(path, 0o120000, oid, &meta));
			}
			Some(meta) if meta.kind.is_file() => {
				let content = self.work().read(path)?;
				let oid = self.repo.write_blob(&content).await?;
				// Preserve a tracked file's executable bit when the capability cannot report it (WASI):
				// staging an otherwise-unchanged executable must not silently downgrade it to `100644` —
				// git's `core.fileMode=false`. A newly tracked file has no prior mode, so it defaults to
				// `100644` (git records new files without a trusted exec bit the same way).
				let expected = index
					.entry(path)
					.map(|existing| existing.mode)
					.unwrap_or(0o100644);
				index.remove_type_conflicts(path);
				index.upsert(entry(path, effective_mode(&meta, expected), oid, &meta));
			}
			Some(_) => {}
			None => index.remove(path),
		}
		Ok(())
	}

	/// Stage deletions for a directory pathspec: any tracked entry under `dir_rel` (the empty string
	/// is the work-tree root, matching everything) whose working-tree file no longer exists is removed
	/// from the index. This is git 2.0+ `add <dir>` / `add .` behaviour — a walk stages the files that
	/// are present, and this pass stages the removals of those that vanished, so `rm foo && add .`
	/// records the deletion rather than silently keeping the stale entry. A single explicit pathspec
	/// that is absent is handled directly by [`Self::add`]; this pass covers the directory case.
	fn stage_deletions(&self, index: &mut Index<H>, dir_rel: &str) -> Result<(), WorktreeError> {
		let prefix = if dir_rel.is_empty() {
			String::new()
		} else {
			format!("{dir_rel}/")
		};
		// Snapshot the candidate paths first, since the `lstat` loop mutates the index.
		let candidates: Vec<String> = index
			.entries
			.iter()
			.filter(|entry| entry.stage == 0 && entry.path.starts_with(&prefix))
			.map(|entry| entry.path.clone())
			.collect();
		for path in candidates {
			if self.work().lstat(&path)?.is_none() {
				index.remove(&path);
			}
		}
		Ok(())
	}

	/// Compute the three-way status: HEAD tree vs index (staged) and index vs
	/// working tree (unstaged), plus untracked files.
	pub async fn status(&self) -> Result<Status, WorktreeError> {
		crate::status::compute(self).await
	}

	/// Stage-0 tracked paths present on disk whose content or mode diverges from the index, verified by
	/// **always hashing** the working file rather than trusting the index stat cache. A removal-safety
	/// re-verification that catches edits `status()` can miss (a stat-preserving/same-size rewrite, a
	/// coarse-timestamp filesystem) and skip-worktree edits `status()` omits entirely. See
	/// [`crate::status::diverged_tracked_content`].
	pub async fn diverged_tracked_content_paths(&self) -> Result<Vec<String>, WorktreeError> {
		crate::status::diverged_tracked_content(self).await
	}

	/// Whether the index carries any staged (index-vs-`HEAD`) or unmerged change, computed **without touching
	/// the working tree** — valid even when the checkout is gone. Safe removal of a checkout-missing partial
	/// uses this to refuse dropping an admin whose index holds staged/conflicted work. See
	/// [`crate::status::has_staged_changes`].
	pub async fn has_staged_changes(&self) -> Result<bool, WorktreeError> {
		crate::status::has_staged_changes(self).await
	}

	/// Whether the index is a **sparse index** (`git sparse-checkout --cone --sparse-index`): it carries a
	/// *sparse-directory entry* — a `040000` (tree) mode entry that stands in for a whole collapsed out-of-cone
	/// directory instead of its individual blobs. gitana does not expand these, so `status()` would compare the
	/// collapsed directory against the expanded HEAD tree and report spurious add/delete pairs. Callers that
	/// must reason about the working tree (e.g. safe removal) use this to refuse honestly rather than trust that
	/// bogus status. A normal index never contains a `040000` entry, so this is a reliable discriminator.
	pub async fn is_sparse_index(&self) -> Result<bool, WorktreeError> {
		let index = self.load_index().await?;
		Ok(index.entries.iter().any(|e| e.mode & 0o170000 == 0o040000))
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

fn entry<H: HashAlgorithm>(path: &str, mode: u32, oid: ObjectId<H>, meta: &Meta) -> IndexEntry<H> {
	IndexEntry {
		stat: stat_of(meta),
		mode,
		oid,
		stage: 0,
		assume_valid: false,
		skip_worktree: false,
		path: path.to_owned(),
	}
}

/// Whether a working-tree file matches an index entry by its stat cache and mode
/// (the fast path that avoids re-hashing).
pub(crate) fn stat_matches<H: HashAlgorithm>(entry: &IndexEntry<H>, meta: &Meta) -> bool {
	entry.mode == mode_of(meta) && entry.stat == stat_of(meta)
}

/// Collect all non-ignored files under `dir_rel` (recursively), applying `.gitignore` and skipping
/// `.git`. Used to expand a directory pathspec for `add`.
fn walk_files<W: WorkDirFs>(
	work: &W,
	dir_rel: &str,
	stack: &mut Vec<DirIgnore>,
	out: &mut Vec<String>,
) -> Result<(), WorktreeError> {
	let pushed = push_gitignore(work, dir_rel, stack)?;
	for entry in work.read_dir(dir_rel)? {
		if entry.name == ".git" {
			continue;
		}
		let rel = join_rel(dir_rel, &entry.name);
		let is_dir = entry.kind.is_dir();
		if ignore::is_ignored(&rel, is_dir, stack) {
			continue;
		}
		if is_dir {
			walk_files(work, &rel, stack, out)?;
		} else {
			out.push(rel);
		}
	}
	if pushed {
		stack.pop();
	}
	Ok(())
}
