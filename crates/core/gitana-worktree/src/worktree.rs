use std::path::{Path, PathBuf};

use gitana_file_store::{FileStore, FileStoreError, WriteOutcome};
use gitana_file_store_local::{Meta, WorkDirFs};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

use crate::excludes::StandardExcludes;
use crate::fsmeta::{effective_mode, join_rel, mode_of, push_gitignore, stat_of};
use crate::ignore::{self, DirIgnore};
use crate::index_lock::IndexLock;
use crate::sparse::{SparseCheckout, SparseReapply, SparseSet};
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

	/// Whether the `.git/index` file exists. A *missing* index (distinct from a present-but-empty one, both
	/// of which [`Self::load_index`] returns as an empty index) has no staged state to preserve, so a
	/// two-tree-merge checkout falls back to a full authoritative checkout that rebuilds from the target.
	pub(crate) async fn index_exists(&self) -> Result<bool, WorktreeError> {
		// Ask the store whether the file exists rather than reading (and discarding) the whole index —
		// `merge_apply` re-reads it via `load_index` immediately after, so a full read here would double
		// the index I/O and allocation on every non-force switch.
		Ok(self.files().exists("index").await?)
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

	/// The active sparse-checkout matcher for this worktree, or `None` when sparse-checkout is disabled.
	/// A tracked file the matcher does not [`include`](SparseCheckout::includes) is omitted from the
	/// working tree (its index entry carries the skip-worktree bit).
	///
	/// `core.sparseCheckout` / `core.sparseCheckoutCone` are **per-worktree** settings: git turns on
	/// `extensions.worktreeConfig` and stores them in `config.worktree` (even for an ordinary
	/// repository — probed against git 2.50.1), so they are read from there first, falling back to the
	/// merged effective config for a hand-set value. The patterns come from the per-worktree
	/// `info/sparse-checkout`; an **absent** file means no narrowing — probed against git 2.50.1, a
	/// `core.sparseCheckout=true` with no pattern file does a full checkout — so it is treated as
	/// sparse-inactive (`None`), not an all-excluding matcher.
	pub(crate) async fn sparse_checkout(&self) -> Result<Option<SparseCheckout>, WorktreeError> {
		if !self.sparse_config_bool("sparsecheckout").await? {
			return Ok(None);
		}
		let cone = self.sparse_config_bool("sparsecheckoutcone").await?;
		// `core.ignoreCase` is a repository-global setting read from the standard config stack (not the
		// per-worktree override): with it on, git matches sparse patterns case-insensitively — the norm on
		// macOS/Windows, where `git init` sets it. Probed against git 2.50.1 for both cone and non-cone.
		let ignorecase = self
			.repo
			.effective_config()
			.await?
			.get_bool_validated("core", None, "ignorecase")?
			.unwrap_or(false);
		let text = match self.files().read_path("info/sparse-checkout").await {
			Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
			// No pattern file → no sparse narrowing (git does a full checkout), so sparse is inactive.
			Err(FileStoreError::NotFound) => return Ok(None),
			Err(error) => return Err(error.into()),
		};
		Ok(Some(SparseCheckout::parse(&text, cone, ignorecase)))
	}

	/// Read a per-worktree sparse `core.<name>` bool: the `config.worktree` override (where git and
	/// `gta sparse-checkout` store it) if present, else the merged effective config; absent → `false`.
	///
	/// git honours `config.worktree` **only** when the common config enables `extensions.worktreeConfig`
	/// (a repository-format extension); a stale or hand-written `config.worktree` without the extension is
	/// ignored, and the value falls back to the merged config. This gates that override, matching
	/// [`crate::status::worktree_file_mode`]'s handling of `core.fileMode`.
	async fn sparse_config_bool(&self, name: &str) -> Result<bool, WorktreeError> {
		let worktree_config_enabled = match self.repo.read_config().await {
			Ok(common) => common
				.get_bool_validated("extensions", None, "worktreeconfig")?
				.unwrap_or(false),
			// An unreadable common config cannot establish the extension; do not honour config.worktree.
			Err(_) => false,
		};
		if worktree_config_enabled {
			match self.files().read_path("config.worktree").await {
				Ok(bytes) => {
					// A non-UTF-8 `config.worktree` is pathological; skip the override rather than fail the read.
					if let Ok(text) = String::from_utf8(bytes) {
						let over = gitana_config::GitConfig::parse(&text)?;
						if let Some(value) = over.get_bool_validated("core", None, name)? {
							return Ok(value);
						}
					}
				}
				Err(FileStoreError::NotFound) => {}
				Err(error) => return Err(error.into()),
			}
		}
		Ok(
			self
				.repo
				.effective_config()
				.await?
				.get_bool_validated("core", None, name)?
				.unwrap_or(false),
		)
	}

	/// Apply the current sparse-checkout patterns to the working tree and index — git's
	/// `sparse-checkout reapply` (and the tail of `set`/`add`/`init`). For every stage-0 tracked file:
	/// - **included but currently omitted** (skip-worktree set): materialise the blob and clear the bit;
	/// - **excluded, present and clean**: remove it from the working tree and set the skip-worktree bit;
	/// - **excluded, present and modified**: leave it (no data loss), do *not* set the bit, and record it
	///   in [`SparseReapply::left_dirty`] — matching git, which warns and leaves such paths.
	///
	/// A no-op returning an empty outcome when sparse-checkout is disabled. Takes the index lock for the
	/// whole update, committing on success (releasing it on error, leaving the tree unchanged).
	pub async fn reapply_sparse(&self) -> Result<SparseReapply, WorktreeError> {
		let Some(matcher) = self.sparse_checkout().await? else {
			return Ok(SparseReapply::default());
		};
		let file_mode = crate::status::worktree_file_mode(self).await;
		let lock = self.lock_index().await?;
		// `apply_sparse` marks `lock` itself, at its first working-tree write (after the fallible index load) —
		// so a failure while loading/inspecting the index still releases the lock cleanly.
		match self.apply_sparse(&matcher, file_mode, &lock).await {
			Ok((index, outcome)) => {
				self.commit_index(lock, &index).await?;
				Ok(outcome)
			}
			Err(error) => {
				self.release_index_lock(lock).await;
				Err(error)
			}
		}
	}

	/// The index-mutating core of [`reapply_sparse`](Self::reapply_sparse), run under the held index lock:
	/// returns the updated index (for the caller to commit) and the reapply outcome.
	async fn apply_sparse(
		&self,
		matcher: &SparseCheckout,
		file_mode: bool,
		lock: &IndexLock<'_, F>,
	) -> Result<(Index<H>, SparseReapply), WorktreeError> {
		// Marks `lock` as mutating right before the FIRST working-tree write below — after `load_index` and
		// each entry's fallible content hash, so a pre-mutation failure (a malformed index, an unreadable
		// file) still releases the lock cleanly rather than stranding it. Index-only bit flips don't count:
		// they are never written until the caller commits, so they leave no half-applied tree.
		let mut index = self.load_index().await?;
		let mut left_dirty = Vec::new();
		let mut not_updated = Vec::new();
		for entry in index.entries.iter_mut() {
			if entry.stage != 0 {
				continue;
			}
			// Classify the on-disk file by HASHING it (never the stat fast path), so a stat-preserving or
			// coarse-timestamp edit cannot be mistaken for clean and destroyed. git preserves a modified file
			// on both sides — it never overwrites or deletes local content to satisfy the patterns.
			let state = crate::status::worktree_content_state(self, entry, file_mode).await?;
			if matcher.includes(&entry.path) {
				// Included: materialise it if it is currently omitted (skip-worktree set).
				if entry.skip_worktree {
					match state {
						// A modified file already sits at the path: git keeps it and clears the bit — the path is
						// now IN the cone, so its edit is an ordinary modification, NOT a "left despite sparse
						// patterns" case (that list is reserved for excluded paths). Do not warn.
						crate::status::WorktreeContent::Diverged => {}
						// Absent: materialise from the blob — unless an untracked file occupies an ancestor slot,
						// in which case git leaves that file, writes nothing, and reports the path as "not
						// updated" (it still clears the bit below, so the path shows as deleted in status).
						crate::status::WorktreeContent::Absent => {
							if crate::checkout::ancestor_blocked(self.work(), &entry.path)? {
								not_updated.push(entry.path.clone());
							} else {
								let mode = format!("{:o}", entry.mode);
								let (path, oid) = (entry.path.clone(), entry.oid);
								lock.mark_mutation_started();
								crate::checkout::write_worktree_file(self, &path, &mode, oid).await?;
							}
						}
						crate::status::WorktreeContent::Reconstructable => {}
					}
					entry.skip_worktree = false;
				}
			} else {
				// Excluded by the matcher. Reconcile the on-disk file regardless of the current bit, so a file
				// recreated at an already-omitted path is not left hidden:
				match state {
					// Present and modified: git never hides it — clear the bit and record it (git's "left
					// despite sparse patterns" warning), whether the bit was already set or newly excluded.
					crate::status::WorktreeContent::Diverged => {
						entry.skip_worktree = false;
						left_dirty.push(entry.path.clone());
					}
					// Absent: omit it (set the bit; a no-op when already omitted).
					crate::status::WorktreeContent::Absent => entry.skip_worktree = true,
					// Present but reconstructable: remove the clean file and omit it. A reconstructable gitlink
					// mount (an empty submodule directory) is rmdir'd via the mode-aware helper — `remove_worktree_path`
					// never removes a directory, so the mount would linger while the index records it skip-worktree.
					crate::status::WorktreeContent::Reconstructable => {
						lock.mark_mutation_started();
						if entry.mode == 0o160000 {
							crate::checkout::remove_gitlink_mount(self, &entry.path)?;
						} else {
							crate::checkout::remove_worktree_path(self, &entry.path)?;
						}
						entry.skip_worktree = true;
					}
				}
			}
		}
		Ok((
			index,
			SparseReapply {
				left_dirty,
				not_updated,
			},
		))
	}

	/// Persist and apply the sparse-checkout `set` (git's `sparse-checkout set`/`init`, and — after the
	/// caller merges the current set — `add`): enable git's config (`extensions.worktreeConfig` in the
	/// common config; `core.sparseCheckout` [+ `core.sparseCheckoutCone` in cone mode] per-worktree),
	/// write `.git/info/sparse-checkout` in the mode's format, then reapply. Returns the reapply outcome
	/// (paths left in the working tree because they had local modifications).
	pub async fn apply_sparse_set(&self, set: &SparseSet) -> Result<SparseReapply, WorktreeError> {
		// Take the index lock BEFORE writing any config or pattern file, so a held `index.lock` fails the
		// whole operation before it changes anything — rather than leaving the new patterns/config on disk
		// with the working tree and index still reflecting the old set (git rejects up front too).
		let file_mode = crate::status::worktree_file_mode(self).await;
		let lock = self.lock_index().await?;
		let result: Result<(Index<H>, SparseReapply), WorktreeError> = async {
			// Enabling the config and writing the pattern file are the first PERSISTENT changes, so mark before
			// them (not just before the worktree reconciliation): a cancellation that has already written
			// `config.worktree` / `info/sparse-checkout` must not release `index.lock` and let a successor act
			// on a half-enabled sparse state. Fail closed from here.
			lock.mark_mutation_started();
			self.write_sparse_enabled(true, set.is_cone()).await?;
			self
				.files()
				.write_path_replace("info/sparse-checkout", set.render().as_bytes())
				.await?;
			// Apply under the held lock. The set was just enabled, so the matcher is present; a defensive
			// `None` (e.g. an empty file) is a no-op. `apply_sparse` marks again at its first worktree write.
			match self.sparse_checkout().await? {
				Some(matcher) => self.apply_sparse(&matcher, file_mode, &lock).await,
				None => Ok((self.load_index().await?, SparseReapply::default())),
			}
		}
		.await;
		match result {
			Ok((index, outcome)) => {
				self.commit_index(lock, &index).await?;
				Ok(outcome)
			}
			Err(error) => {
				self.release_index_lock(lock).await;
				Err(error)
			}
		}
	}

	/// Materialise `paths` from `tree` into the working tree, **bypassing** sparse-checkout. Used to
	/// vivify conflicted (unmerged) paths: git always makes a conflict visible and resolvable regardless
	/// of the sparse patterns (skip-worktree is incompatible with an unmerged entry), so a merge whose
	/// conflict falls on an out-of-cone path must still write its marker file. A path absent from `tree`
	/// is skipped.
	pub async fn materialise_paths(
		&self,
		tree: ObjectId<H>,
		paths: &[String],
	) -> Result<(), WorktreeError> {
		let entries = self.repository().read_tree(tree).await?;
		for (path, mode, oid) in &entries {
			if paths.iter().any(|p| p == path) {
				// A gitlink conflict path: git records the base/ours/theirs stages but NEVER materialises the
				// mount for a conflict — it leaves the slot exactly as it is. A present mount (populated or an
				// empty mount directory) is preserved; a NON-DIRECTORY the user has there (file, symlink, FIFO,
				// socket, device) is local data left in place; and an ABSENT mount stays ABSENT (probed vs git
				// 2.55: a divergent-pointer conflict with no checkout leaves `sub` absent, so `add .` resolves
				// it as `D sub` rather than erroring on a stray empty mount). So skip the mount write entirely
				// here. (In a plain checkout `write_worktree_file` DOES create/replace the mount, which is
				// correct there — this whole-slot preservation is specific to conflicts.)
				if mode == "160000" {
					continue;
				}
				crate::checkout::write_worktree_file(self, path, mode, *oid).await?;
			}
		}
		Ok(())
	}

	/// The currently-configured sparse-checkout set — the included directories (cone) or the pattern
	/// lines (non-cone) — or `None` when sparse-checkout is not enabled. Drives `list` and the read half
	/// of `add`.
	pub async fn current_sparse_set(&self) -> Result<Option<SparseSet>, WorktreeError> {
		let Some(matcher) = self.sparse_checkout().await? else {
			return Ok(None);
		};
		let text = match self.files().read_path("info/sparse-checkout").await {
			Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
			Err(FileStoreError::NotFound) => String::new(),
			Err(error) => return Err(error.into()),
		};
		Ok(Some(match matcher {
			// `list` recovers the directories; case-folding is irrelevant to `dirs()`, so pass `false`.
			SparseCheckout::Cone(_) => SparseSet::Cone(crate::sparse::Cone::parse(&text, false).dirs()),
			SparseCheckout::NonCone(_) => SparseSet::NonCone(text.lines().map(str::to_owned).collect()),
		}))
	}

	/// Disable sparse-checkout (git's `sparse-checkout disable`): materialise every omitted
	/// (skip-worktree) file and clear its bit, then set the config booleans false. `extensions.
	/// worktreeConfig` and `.git/info/sparse-checkout` are left in place, exactly as git leaves them.
	pub async fn disable_sparse(&self) -> Result<SparseReapply, WorktreeError> {
		let file_mode = crate::status::worktree_file_mode(self).await;
		let lock = self.lock_index().await?;
		let outcome = match self.materialise_all_sparse(file_mode, &lock).await {
			Ok((index, outcome)) => {
				self.commit_index(lock, &index).await?;
				outcome
			}
			Err(error) => {
				self.release_index_lock(lock).await;
				return Err(error);
			}
		};
		self.write_sparse_enabled(false, false).await?;
		Ok(outcome)
	}

	/// Materialise every stage-0 skip-worktree entry into the working tree and clear its bit, returning
	/// the updated index and reapply outcome. Run under the held index lock by [`Self::disable_sparse`].
	/// A file the user placed at an omitted path is preserved, not overwritten — git keeps it (the
	/// omitted file is normally absent, so the common case writes the blob); an untracked file occupying
	/// an ancestor slot blocks the write (recorded in `not_updated`), and the bit is cleared regardless.
	async fn materialise_all_sparse(
		&self,
		file_mode: bool,
		lock: &IndexLock<'_, F>,
	) -> Result<(Index<H>, SparseReapply), WorktreeError> {
		// Marks `lock` right before the FIRST working-tree write below — after the fallible index load and
		// per-entry content hash — so a pre-mutation failure releases the lock cleanly.
		let mut index = self.load_index().await?;
		let mut not_updated = Vec::new();
		for entry in index.entries.iter_mut() {
			if entry.stage == 0 && entry.skip_worktree {
				if matches!(
					crate::status::worktree_content_state(self, entry, file_mode).await?,
					crate::status::WorktreeContent::Absent
				) {
					if crate::checkout::ancestor_blocked(self.work(), &entry.path)? {
						not_updated.push(entry.path.clone());
					} else {
						let mode = format!("{:o}", entry.mode);
						let (path, oid) = (entry.path.clone(), entry.oid);
						lock.mark_mutation_started();
						crate::checkout::write_worktree_file(self, &path, &mode, oid).await?;
					}
				}
				entry.skip_worktree = false;
			}
		}
		Ok((
			index,
			SparseReapply {
				left_dirty: Vec::new(),
				not_updated,
			},
		))
	}

	/// Write git's sparse-checkout config: `extensions.worktreeConfig = true` in the common config (git
	/// honours the extension only from the local/common file, and never clears it on disable), and
	/// `core.sparseCheckout` / `core.sparseCheckoutCone` per-worktree — plus `index.sparse = false` when
	/// disabling, which git records (gitana has no sparse-index; the key is inert but kept for fidelity).
	async fn write_sparse_enabled(&self, enabled: bool, cone: bool) -> Result<(), WorktreeError> {
		let mut common = self.repository().read_config().await?;
		common.set("extensions", None, "worktreeConfig", "true")?;
		self.repository().write_config(&common).await?;

		// `core.sparseCheckout`/`Cone` are per-worktree; read-modify-write to keep any other keys.
		let mut over = match self.files().read_path("config.worktree").await {
			Ok(bytes) => {
				let text = String::from_utf8(bytes)
					.map_err(|_| std::io::Error::other("config.worktree is not UTF-8"))?;
				gitana_config::GitConfig::parse(&text)?
			}
			Err(FileStoreError::NotFound) => gitana_config::GitConfig::new(),
			Err(error) => return Err(error.into()),
		};
		let flag = |value: bool| if value { "true" } else { "false" };
		over.set("core", None, "sparseCheckout", flag(enabled))?;
		over.set("core", None, "sparseCheckoutCone", flag(cone))?;
		if !enabled {
			over.set("index", None, "sparse", "false")?;
		}
		self
			.files()
			.write_path_replace("config.worktree", over.render().as_bytes())
			.await?;
		Ok(())
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
	pub(crate) async fn lock_index(&self) -> Result<IndexLock<'_, F>, WorktreeError> {
		match self.files().write_path_if_absent("index.lock", &[]).await? {
			WriteOutcome::Written => Ok(IndexLock::new(self.files())),
			WriteOutcome::AlreadyExists => Err(WorktreeError::IndexLocked),
		}
	}

	/// Write `index` while holding `lock`, then release the lock — atomically. The index is replaced by
	/// a write-to-temp-then-rename (a reader never sees a torn index) and `index.lock` is removed in the
	/// *same* blocking step, which runs to completion even if this future is cancelled. That atomicity is
	/// what makes the commit cancellation-safe: a cancelled commit can neither strand `index.lock` nor
	/// release it before the write lands — releasing early would let another writer take the lock and race
	/// the still-completing (uncancellable) write, losing an update. The lock is removed even if the write
	/// fails, so a failure does not strand it.
	pub(crate) async fn commit_index(
		&self,
		mut lock: IndexLock<'_, F>,
		index: &Index<H>,
	) -> Result<(), WorktreeError> {
		// Disarm the `Drop` backstop and hand the lock's release to `replace_and_release_lock`. Everything
		// from here to that call's blocking hand-off is synchronous (no `.await`), and the store performs no
		// `.await` before committing the write-and-unlink to run, so cancellation cannot slip in after
		// disarming but before the atomic step is guaranteed — closing the window a separate write-then-delete
		// would leave open (the write outlives cancellation on the blocking pool, but the delete would not).
		lock.disarm();
		self
			.files()
			.replace_and_release_lock("index", &index.write_v4(), "index.lock")
			.await?;
		Ok(())
	}

	/// Release a held index lock (from [`Self::lock_index`]) without writing, removing `index.lock`.
	/// Use when an operation fails after locking but before [`Self::commit_index`], so it does not
	/// leave a stale lock behind.
	pub(crate) async fn release_index_lock(&self, mut lock: IndexLock<'_, F>) {
		// Remove the on-disk lock with a single SYNCHRONOUS unlink (the primitive the guard's `Drop` uses),
		// then disarm — but ONLY if the working tree is still untouched. An operation that failed AFTER it
		// began mutating leaves a half-applied tree, so its lock must stay (fail-closed), exactly as the `Drop`
		// backstop keeps it; only a failure before any worktree write releases cleanly. Deliberately NOT the
		// async `delete_path`: its `spawn_blocking` unlink can outlive a cancellation of this future and,
		// because it unlinks unconditionally (`expected: None`), later remove a *successor's* freshly-taken
		// `index.lock` — breaking mutual exclusion and risking a lost index update. A synchronous unlink
		// completes in one uncancellable step while we still hold the lock, so no successor can interleave.
		if !lock.mutation_started() {
			self.files().remove_lock_file_sync("index.lock");
		}
		lock.disarm();
	}

	/// Stage `pathspecs`, interpreted relative to `prefix` (a `/`-joined work-tree-relative
	/// subdirectory, empty at the root). A file is staged directly; a directory (or `.`) is
	/// walked, applying `.gitignore`, and its non-ignored files are staged; a path that no
	/// longer exists is removed from the index (a staged deletion).
	///
	/// Pathspecs that cannot be fully staged are reported afterwards via
	/// [`WorktreeError::PathspecAdvisory`] (git's exit-nonzero advice, after the stageable work is saved),
	/// carrying two lists a front-end renders as git's blocks. **Ignored** is a pathspec-*level* diagnostic
	/// collected up front over EVERY element (positive OR negative) by [`Self::collect_ignored_advisory`]:
	/// any element whose literal — or glob base — is or lies under an *existing* ignored path is reported,
	/// collapsed to where the rule matched (`ign/new` under an ignored `ign/` → `ign`), independent of
	/// exclusion, tracked status, or leaf existence; a broad `.`/glob sweep never advises. Staging itself
	/// still refuses only *untracked* ignored content (a tracked modification beneath an ignored path is
	/// staged, since ignore never applies to a tracked path); `force` (git's `-f`/`--force`) skips the whole
	/// ignored pass and stages the untracked content too. **Sparse** lists the pathspecs that matched
	/// out-of-cone paths (git's `--sparse` advice), in argument/discovery order. Probed vs git 2.50.1.
	///
	/// DELIBERATE divergences from stock git (the staged index, the ignored-path advisory, and a
	/// single-kind sparse advisory otherwise match git byte-for-byte across a differential fuzz of the
	/// pathspec surface):
	///
	/// 1. **Exclude + non-root positive.** git has a quirk where a *non-root* positive pathspec (a glob or
	///    named directory, e.g. `new/*.rs`) combined with ANY negative pathspec suppresses staging of the
	///    *untracked* files that positive matches — `add 'new/*.rs' ':!nope'` stages nothing, and
	///    `add 'new/*.rs' ':!new/*.rs'` even reports the positive unmatched. A broad `.` positive is exempt.
	///    gitana instead stages those untracked files (the intuitive result), a documented choice.
	/// 2. **Sparse advisory in mixed/repeated out-of-cone adds.** git emits the "outside sparse-checkout"
	///    advisory as *separate per-pass blocks* — one for tracked skip-worktree pathspecs (argument order,
	///    duplicates preserved) and a distinct one for discovered/untracked out-of-cone paths (sorted,
	///    de-duplicated), tracked block first (probed vs git 2.50.1: `add out/a out/new` prints two
	///    blocks). gitana renders a single merged block, so an `add` mixing tracked and untracked
	///    out-of-cone paths (or repeating an untracked one, or several from a walk) may order/deduplicate
	///    them differently. A single-kind sparse advisory — and the ignored-path advisory in every case —
	///    matches git byte-for-byte. (See `TODO.md`.)
	pub async fn add(
		&self,
		pathspecs: &[&str],
		prefix: &str,
		force: bool,
		excludes_file: Option<&str>,
	) -> Result<(), WorktreeError> {
		let mut index = self.load_index().await?;
		// `add` with no pathspec at all is git's "Nothing specified, nothing added" no-op: it reads no
		// exclude files (so a directory `.git/info/exclude` is not fatal here) but *does* validate
		// `core.ignoreCase` (probed vs git 2.55). Callers reaching this crate directly — the wasm
		// component, MCP, other API users — can pass an empty list even though the `gta` CLI requires
		// paths, so handle it before loading any exclude file.
		if pathspecs.is_empty() {
			crate::excludes::ignore_case(self).await?;
			return self.save_index(&index).await;
		}
		// git's standard excludes for the ignored-path decisions below: the `core.ignoreCase` fold flag
		// plus the whole-tree exclude levels (`core.excludesFile`, `.git/info/exclude`) beneath
		// per-directory `.gitignore`. Seeds every ignore stack so `add` prunes (and advises about) the same
		// files git does; previously only `.gitignore` was consulted, case-sensitively.
		//
		// `add -f` stages ignored paths regardless, and git does not read the exclude files on that path
		// (a directory `.git/info/exclude` is fatal for a plain `add` but not for `add -f`, probed vs git
		// 2.55) — it still validates `core.ignoreCase`, so resolve the fold flag alone and leave the base
		// empty (the forced walk consults no ignore stack anyway).
		let StandardExcludes {
			fold,
			base: excludes,
		} = if force {
			StandardExcludes {
				fold: crate::excludes::ignore_case(self).await?,
				base: Vec::new(),
			}
		} else {
			crate::excludes::standard_excludes(self, excludes_file).await?
		};
		// Tracked submodule (gitlink) mounts the walker prunes (opaque to `add`) — so it never descends into
		// a submodule to stage its contents nor fails on an unreadable child. Folded under `core.ignoreCase`
		// so a case-variant on-disk mount (indexed `Sub`, on disk `sub`) is still matched. Uses the same
		// index-based `gitlink_mount` gate as the dir-arms below: a gitlink stage with no tracked children,
		// whether a pure gitlink OR a same-path blob-vs-gitlink conflict — git treats both as an opaque
		// submodule boundary purely from the index (the on-disk `.git` marker is irrelevant, probed vs git
		// 2.55). Only a subtree conflict (tracked `sub/…` children) stays walkable — its dir is real subtree
		// content `add` descends into.
		let gitlinks: std::collections::HashSet<String> = index
			.entries
			.iter()
			.filter(|entry| gitlink_mount(&index, &entry.path, fold))
			.map(|entry| {
				if fold {
					entry.path.to_ascii_lowercase()
				} else {
					entry.path.clone()
				}
			})
			.collect();
		// The active sparse matcher: `add` never stages a path outside it (git refuses an out-of-cone path,
		// advising `--sparse`), whether or not the path already has a skip-worktree entry.
		let sparse = self.sparse_checkout().await?;
		// git stages every in-cone change, writes the index, and only THEN exits nonzero with its
		// "outside sparse-checkout" advice for every pathspec that matched an out-of-cone path it could not
		// add (probed vs git 2.50.1). So collect those omissions in argument/discovery order and, after
		// saving, surface them — rather than aborting before the in-cone work is persisted. The trigger is
		// an *untracked* out-of-cone file matched by any pathspec, or an out-of-cone path named *explicitly*
		// (a literal, or a glob rooted out-of-cone); a broad pathspec that merely sweeps over a tracked
		// skip-worktree entry skips it silently.
		let mut sparse_omitted: Vec<String> = Vec::new();
		// A `:(exclude)` pathspec subtracts from what the positives stage; a set with only negatives stages
		// the whole tree minus them, exactly like `add .` with the exclusions applied.
		let set = crate::pathspec::PathspecSet::parse(pathspecs, prefix)?;
		// git's ignored-path advisory is a pathspec-level diagnostic, independent of what is staged: EVERY
		// explicitly-named pathspec (positive or negative, whatever its exclusion/tracked/existence status)
		// whose literal — or glob base — is or lies under an *existing* ignored path is reported, collapsed
		// to where the rule matched. Collect those up front, once, over the whole set (probed vs git 2.50.1;
		// `force` opts out entirely). The staging arms below only decide what to stage; they no longer record
		// the advisory.
		let ignored = if force {
			Vec::new()
		} else {
			self.collect_ignored_advisory(&set, &index, &excludes, fold)?
		};
		let mut ignore_stack: Vec<DirIgnore> = excludes.clone();
		if set.is_positive_empty() {
			// A set of *only* negatives (the empty-pathspec case having already returned above) stages the
			// whole tree minus them, like `add .` with the exclusions applied.
			let mut files = Vec::new();
			walk_files(
				self.work(),
				"",
				&mut ignore_stack,
				&mut files,
				&gitlinks,
				force,
				fold,
			)?;
			let walked: std::collections::HashSet<String> = files.iter().cloned().collect();
			for file in &files {
				if !set.is_excluded(file) {
					self
						.stage_walked(&mut index, file, sparse.as_ref(), &mut sparse_omitted, fold)
						.await?;
				}
			}
			self
				.stage_tracked_outside_walk(&mut index, "", sparse.as_ref(), &set, &walked, fold)
				.await?;
			self.save_index(&index).await?;
			return finish_advisory(sparse_omitted, ignored);
		}
		// git decides whether each positive matched a tracked path against the index *as it was before any
		// staging* — so overlapping specs such as `add gone gone` or `add . gone` (where the first occurrence
		// stages `gone`'s deletion, removing the entry) do not make the later occurrence report "did not
		// match". Snapshot the initially-tracked (non-sparse) and unmerged paths for the match check below.
		let initial_tracked: std::collections::HashSet<String> = index
			.entries
			.iter()
			.filter(|entry| entry.stage == 0 && !entry.skip_worktree)
			.map(|entry| entry.path.clone())
			.chain(index.unmerged_paths().map(str::to_owned))
			.collect();
		// The same snapshot including **out-of-cone (skip-worktree)** entries — a glob matches those too
		// (they feed the sparse advice), and a repeated glob matches a path an earlier spec removed.
		let initial_tracked_all: std::collections::HashSet<String> = index
			.entries
			.iter()
			.filter(|entry| entry.stage == 0)
			.map(|entry| entry.path.clone())
			.chain(index.unmerged_paths().map(str::to_owned))
			.collect();
		for (spec, pathspec) in set.positives() {
			// A positive that can never match (a magic path resolving to root, `:/.`) is a no-op for `add`
			// (git stages nothing; `rm`/`restore` instead report it unmatched, via `matches` returning false).
			if pathspec.is_never_matching() {
				continue;
			}
			// A glob pathspec (and an `:(icase)` one, whose match must resolve to the actual worktree path
			// rather than the spec's spelling) walks its base directory and stages every present file it
			// matches, plus the deletion of any matching tracked entry whose file is gone. Unlike an
			// explicitly-named directory it silently skips out-of-cone matches (like a broad `.` walk) and
			// never triggers the out-of-cone-directory refusal — probed vs git 2.50.1.
			if !pathspec.is_literal() || pathspec.is_icase() {
				self
					.add_glob(
						&mut index,
						pathspec,
						spec,
						sparse.as_ref(),
						&set,
						&initial_tracked_all,
						&mut sparse_omitted,
						force,
						&excludes,
						fold,
					)
					.await?;
				continue;
			}
			let rel = pathspec.as_str().to_owned();
			let dir_only = pathspec.dir_only();
			// The empty spec (`.` at the work-tree root) always names the root directory to walk.
			if rel.is_empty() {
				let mut files = Vec::new();
				walk_files(
					self.work(),
					"",
					&mut ignore_stack,
					&mut files,
					&gitlinks,
					force,
					fold,
				)?;
				let walked: std::collections::HashSet<String> = files.iter().cloned().collect();
				for file in &files {
					if !set.is_excluded(file) {
						self
							.stage_walked(&mut index, file, sparse.as_ref(), &mut sparse_omitted, fold)
							.await?;
					}
				}
				self
					.stage_tracked_outside_walk(&mut index, "", sparse.as_ref(), &set, &walked, fold)
					.await?;
				continue;
			}
			// An EXPLICITLY-named path inside a tracked submodule is git's fatal (exit 128): the superproject
			// cannot stage a submodule's own contents. (A broad walk prunes the mount silently; only a literal
			// path naming INTO it errors — probed vs git 2.55: `add sub/f` → "Pathspec 'sub/f' is in submodule
			// 'sub'".)
			if let Some(submodule) = gitlink_ancestor(&index, &rel, fold) {
				return Err(WorktreeError::PathspecInSubmodule {
					path: rel,
					submodule,
				});
			}
			match self.work().lstat(&rel)? {
				Some(meta) if meta.kind.is_dir() => {
					let mut files = Vec::new();
					// Seed the ignore stack with the ancestor `.gitignore`s (root down to `rel`'s parent)
					// so a rule above the named directory still applies to its walk.
					let mut stack = crate::checkout::ignore_prefix(self.work(), &rel, &excludes)?;
					// Don't stage *untracked* files from an explicitly-named ignored directory (or one under an
					// ignored ancestor): git refuses them, staging only the tracked modifications beneath it
					// (handled by `stage_tracked_outside_walk` below). `force` stages the untracked content too.
					// The advisory itself is recorded by the up-front `collect_ignored_advisory` pass; here we
					// only decide whether to walk. Probed vs git 2.50.1: `add ignored` on an ignored `ignored/`
					// stages a modified tracked child but not an untracked sibling, and exits non-zero.
					if force || self.ignored_report_path(&rel, &stack, fold)?.is_none() {
						walk_files(
							self.work(),
							&rel,
							&mut stack,
							&mut files,
							&gitlinks,
							force,
							fold,
						)?;
					}
					files.retain(|file| !set.is_excluded(file));
					// An explicitly-named directory whose matches are ALL out-of-cone is one git reports (the
					// deferred sparse advice), unlike a broad `.` walk that silently skips such paths, or a
					// directory with any in-cone content (which stages that and skips the out-of-cone siblings).
					if self.only_out_of_cone_dir(&index, &rel, &files, sparse.as_ref()) {
						sparse_omitted.push(rel.clone());
					}
					let walked: std::collections::HashSet<String> = files.iter().cloned().collect();
					for file in &files {
						self
							.stage_walked(&mut index, file, sparse.as_ref(), &mut sparse_omitted, fold)
							.await?;
					}
					self
						.stage_tracked_outside_walk(&mut index, &rel, sparse.as_ref(), &set, &walked, fold)
						.await?;
				}
				// A trailing-slash spec required a directory but resolved to a file.
				Some(_) if dir_only => return Err(WorktreeError::PathspecMatch(spec.to_owned())),
				Some(_) => {
					let out_of_cone = index.is_sparse(&rel)
						|| sparse
							.as_ref()
							.is_some_and(|matcher| !matcher.includes(&rel));
					if out_of_cone {
						// A TRACKED out-of-cone path (any stage) explicitly named is reported BEFORE the
						// exclusion is subtracted — `add out/f :!out/f` still reports the sparse path (probed vs
						// git 2.50.1). An UNTRACKED out-of-cone file instead has the exclusion applied first, so
						// `add out/new.rs :!out/new.rs` is an empty selection and succeeds.
						if index.entry(&rel).is_some() {
							sparse_omitted.push(rel.clone());
							continue;
						}
						if set.is_excluded(&rel) {
							continue;
						}
						sparse_omitted.push(rel.clone());
						continue;
					}
					// An in-cone file the negatives exclude is skipped (git: `add foo :!foo` stages nothing).
					if set.is_excluded(&rel) {
						continue;
					}
					// An explicitly-named UNTRACKED ignored file is not staged, unless `force` — git refuses it
					// (the advisory itself comes from `collect_ignored_advisory`). Ignore never applies to a
					// tracked path, so a tracked (any stage) or unmerged file still stages its modification.
					let tracked =
						index.entry(&rel).is_some() || index.unmerged_paths().any(|path| path == rel);
					if !force && !tracked {
						let stack = crate::checkout::ignore_prefix(self.work(), &rel, &excludes)?;
						if self.ignored_report_path(&rel, &stack, fold)?.is_some() {
							// A `:(glob)` pathspec (glob magic, even without metacharacters) that resolves only to
							// an untracked ignored file is git's "did not match" (exit 128), NOT the ignored advisory
							// a plain literal gets — the glob matched no addable path (probed vs git 2.50.1:
							// `add :(glob)ign/new` did not match, while `add ign/new` advises).
							if pathspec.is_glob() {
								return Err(WorktreeError::PathspecMatch(spec.to_owned()));
							}
							continue;
						}
					}
					self
						.stage_file(&mut index, &rel, sparse.as_ref(), fold)
						.await?
				}
				// Absent from the working tree: stage the deletion of whatever tracked entries the
				// pathspec covers — the exact path (a removed file) and any children (a removed
				// directory, `rm -r dir && add dir`). A spec matching no tracked entry did not match
				// anything; a trailing-slash spec additionally requires it to have named a directory.
				None => {
					// An explicitly-named out-of-cone path (absent by design) is one git reports, like the
					// present case: `git add x/h` on an omitted file exits nonzero with sparse advice, rather
					// than silently succeeding without staging anything. This out-of-cone case takes precedence
					// over both the exclusion skip and the did-not-match check below.
					// Keyed on an actual skip-worktree ENTRY, not the matcher: an absent path that is not tracked
					// (`add out/new.rs`, `out/` out-of-cone, no such entry) is git's "did not match", not the
					// sparse advice (probed vs git 2.50.1). A tracked skip-worktree entry absent by design is.
					if index.is_sparse(&rel) {
						sparse_omitted.push(rel.clone());
						continue;
					}
					// A directory pathspec covering only out-of-cone (skip-worktree) tracked entries is
					// reported the same way (`add b` when the whole `b/` subtree is excluded and absent) —
					// git reports it rather than silently staging nothing.
					if self.only_out_of_cone_dir(&index, &rel, &[], sparse.as_ref()) {
						sparse_omitted.push(rel.clone());
						continue;
					}
					// A sparse-excluded path (or a directory whose only entries are excluded) is
					// invisible to `add`: git treats it as outside the sparse-checkout definition, so it
					// neither matches the pathspec nor has its entry dropped. Ignore sparse entries when
					// deciding whether anything matched, and never remove a sparse entry here.
					let child_prefix = format!("{rel}/");
					// Matched against the pre-staging snapshot (see `initial_tracked`), so an earlier positive
					// that already removed this entry does not turn a later occurrence into "did not match". The
					// snapshot already covers both stage-0 tracked paths and **unmerged** paths (only stages
					// 1/2/3) — `add conflict` on a deleted unmerged path clears its higher stages and records
					// the deletion rather than reporting "did not match" (probed vs git 2.50.1).
					let matched = initial_tracked.contains(&rel)
						|| initial_tracked
							.iter()
							.any(|path| path.starts_with(&child_prefix));
					// A positive matching no tracked path is git's "did not match" — and a negative pathspec
					// does NOT suppress that error (probed vs git 2.50.1: `add missing/ :!missing` and even
					// `add missing` both fail). So the match check precedes the exclusion skip.
					if !matched {
						return Err(WorktreeError::PathspecMatch(spec.to_owned()));
					}
					// Matched a tracked path the negatives exclude — nothing to stage or drop.
					if set.is_excluded(&rel) {
						continue;
					}
					if !index.is_sparse(&rel) {
						index.remove(&rel);
					}
					// The named path is absent, so nothing was walked — every tracked entry under it is a
					// deletion candidate.
					self
						.stage_tracked_outside_walk(
							&mut index,
							&rel,
							sparse.as_ref(),
							&set,
							&std::collections::HashSet::new(),
							fold,
						)
						.await?;
				}
			}
		}
		self.save_index(&index).await?;
		// git stages everything it can, saves, and only THEN exits non-zero with its advice — emitting the
		// sparse block (for out-of-cone pathspecs) and/or the ignored block (for ignored pathspecs), both
		// when both occurred. Surface the two lists together for the front-end to render.
		finish_advisory(sparse_omitted, ignored)
	}

	/// The path git reports as ignored for an explicitly-named `target`: the shallowest ancestor directory
	/// that both **exists** and an ignore rule matches, else `target` itself when it exists and its own
	/// kind is ignored, else `None`. git's `add` advisory reports where the ignore rule pruned an EXISTING
	/// path — `ign/new` (or the non-existent `ign/missing`) under an ignored `ign/` reports the directory
	/// `ign`; a `*.log`-ignored `root.log` reports the file `root.log`; a name whose ignored ancestor does
	/// not exist on disk (`:!foo.log`, or `:!ign` when `ign/` is absent) reports nothing. `stack` must be
	/// seeded (via [`crate::checkout::ignore_prefix`]) with the ancestor `.gitignore`s down to `target`'s
	/// parent. Probed vs git 2.50.1.
	/// Returns `(report, is_leaf)`: `is_leaf` is `true` when the match was `target` itself (its own
	/// ignore rule), `false` when it was an ancestor directory. The distinction matters because git only
	/// advises a *leaf* ignore-match for an UNTRACKED path — a tracked file ignored solely by a leaf
	/// pattern (`root.log` under `*.log`) is staged without advice — whereas an ignored ANCESTOR directory
	/// advises even for a tracked descendant (`ign/tracked` under `ign/`). The caller applies that rule.
	fn ignored_report_path(
		&self,
		target: &str,
		stack: &[DirIgnore],
		fold: bool,
	) -> Result<Option<(String, bool)>, WorktreeError> {
		let mut idx = 0;
		while let Some(next) = target[idx..].find('/') {
			let ancestor = &target[..idx + next];
			// Match against the ancestor's ACTUAL on-disk kind: normally a directory, but git also reports an
			// ignored regular file that a pathspec descends through (`.gitignore` = `file`, `add x :!file/sub`
			// → reports `file`). A dir-only rule (`file/`) then correctly won't match the regular file.
			if let Some(meta) = self.work().lstat(ancestor)?
				&& ignore::is_ignored_fold(ancestor, meta.kind.is_dir(), stack, fold)
			{
				return Ok(Some((ancestor.to_owned(), false)));
			}
			idx += next + 1;
		}
		match self.work().lstat(target)? {
			Some(meta) if ignore::is_ignored_fold(target, meta.kind.is_dir(), stack, fold) => {
				Ok(Some((target.to_owned(), true)))
			}
			_ => Ok(None),
		}
	}

	/// Resolve the literal (pre-wildcard) prefix of an `:(icase)` pathspec to its actual worktree path,
	/// folding ASCII case component by component — git resolves the real path before consulting `.gitignore`,
	/// so `:(icase)IGN/NEW` for an on-disk `ign/new` reports against `ign/new`, not the spec's spelling. The
	/// prefix is the whole normalized path when the spec has no wildcard, else the directory portion before
	/// the first wildcard (`:(icase)IGN/*` → `ign`). Returns `None` when a component has no case-insensitive
	/// match on disk (nothing exists to report). Probed vs git 2.50.1.
	fn resolve_icase_prefix(
		&self,
		pathspec: &crate::pathspec::Pathspec,
	) -> Result<Option<String>, WorktreeError> {
		let normalized = pathspec.as_str();
		let first_wild = normalized
			.bytes()
			.position(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
			.unwrap_or(normalized.len());
		let literal = if first_wild == normalized.len() {
			normalized
		} else {
			match normalized[..first_wild].rfind('/') {
				Some(slash) => &normalized[..slash],
				None => "",
			}
		};
		if literal.is_empty() {
			return Ok(None);
		}
		let components: Vec<&str> = literal.split('/').filter(|part| !part.is_empty()).collect();
		let mut resolved = String::new();
		for (i, component) in components.iter().enumerate() {
			// `resolved` is always a confirmed directory here (the root, or a component the previous
			// iteration verified is a directory), so `read_dir` never hits a non-directory.
			let found = self
				.work()
				.read_dir(&resolved)?
				.into_iter()
				.find(|entry| entry.name.eq_ignore_ascii_case(component));
			let Some(entry) = found else {
				// A missing component stops resolution — the resolved-so-far prefix is returned, so an ignored
				// existing ancestor still reports (`:(icase)IGN/MISSING` → `ign`, like `ign/missing` does).
				break;
			};
			if !resolved.is_empty() {
				resolved.push('/');
			}
			let is_dir = entry.kind.is_dir();
			resolved.push_str(&entry.name);
			// Can't descend past a non-directory (`:(icase)FILE/sub` where `file` is a regular file): stop
			// here, having recorded `file` so an ignore rule on it can still be reported.
			if !is_dir && i + 1 < components.len() {
				break;
			}
		}
		if resolved.is_empty() {
			Ok(None)
		} else {
			Ok(Some(resolved))
		}
	}

	/// Collect git's ignored-path advisory reports over the whole pathspec set. git's advisory is a
	/// pathspec-level diagnostic, independent of staging: EVERY element — positive OR negative — whose
	/// literal (or glob base) is or lies under an existing ignored path is reported, collapsed to where the
	/// rule matched (see [`Self::ignored_report_path`]). So `add ign/tracked` (a tracked file), `add
	/// ign/new :!.` (excluded), and `add :!ign/x` (a negative) all report `ign`, while a broad `.` or a
	/// negative naming a non-ignored path is silent. A *leaf* ignore-match on a TRACKED path is NOT reported
	/// (git stages `add root.log` for a `*.log`-ignored tracked `root.log`, exit 0) — hence `index`.
	/// Deduplicated, first-seen order (the caller sorts). The caller skips this entirely under `force`.
	/// Probed vs git 2.50.1.
	fn collect_ignored_advisory(
		&self,
		set: &crate::pathspec::PathspecSet,
		index: &Index<H>,
		excludes: &[DirIgnore],
		fold: bool,
	) -> Result<Vec<String>, WorktreeError> {
		let mut ignored = Vec::new();
		for pathspec in set.all() {
			if pathspec.is_never_matching() {
				continue;
			}
			// A literal names its whole path; a glob names its literal base directory (`ign/*` → `ign`, an
			// empty base at the root → skip: a broad glob never advises). An `:(icase)` spec is resolved to
			// its actual worktree path first, so a differently-cased spelling still finds the ignored path.
			let icase_probe;
			let glob_base;
			let probe: &str = if pathspec.is_icase() {
				match self.resolve_icase_prefix(pathspec)? {
					Some(resolved) => {
						icase_probe = resolved;
						&icase_probe
					}
					None => continue,
				}
			} else if pathspec.is_literal() {
				pathspec.as_str()
			} else {
				// A glob's literal base, with backslash escapes decoded so an escaped separator
				// (`dir\/foo`) yields `dir/foo` rather than the empty root `base_dir()` returns.
				glob_base = glob_ignore_base(pathspec.as_str());
				&glob_base
			};
			if probe.is_empty() {
				continue;
			}
			let stack = crate::checkout::ignore_prefix(self.work(), probe, excludes)?;
			if let Some((report, is_leaf)) = self.ignored_report_path(probe, &stack, fold)? {
				// A leaf ignore-match on an already-tracked path does not advise — git stages it (ignore never
				// applies to a tracked path, and there is no ignored ancestor directory to report).
				let tracked =
					index.entry(probe).is_some() || index.unmerged_paths().any(|path| path == probe);
				if is_leaf && tracked {
					continue;
				}
				push_unique(&mut ignored, report);
			}
		}
		Ok(ignored)
	}

	/// Stage a glob pathspec: walk its literal base directory and stage every present working-tree file
	/// it matches, then stage the deletion of any matching tracked entry whose file is gone (git 2.0
	/// `add`). Errors with `PathspecMatch` if the glob matched no path at all (git's "did not match any
	/// files"). Out-of-cone matches feed the caller's deferred sparse advice via `omitted`: an untracked
	/// out-of-cone file the walk swept up, or (via `in_cone`) a glob rooted out-of-cone whose every match
	/// lies outside the cone. A sparse (skip-worktree / out-of-cone) entry is never dropped — matching
	/// [`Self::stage_tracked_outside_walk`]. A glob rooted at an *ignored* base (`add 'ign/*'`) stages no
	/// untracked file from it (unless `force`); it succeeds only if it also matched a tracked entry, else it
	/// is git's "did not match". The ignored-path advisory for the base is recorded by the caller's up-front
	/// `collect_ignored_advisory` pass, not here.
	// The parameters are all distinct inputs the glob path genuinely needs (index, matcher, sparse
	// state, the initial-tracked snapshot, and the sparse-omission accumulator); bundling them would obscure
	// more than it helps.
	#[allow(clippy::too_many_arguments)]
	async fn add_glob(
		&self,
		index: &mut Index<H>,
		pathspec: &crate::pathspec::Pathspec,
		spec: &str,
		sparse: Option<&SparseCheckout>,
		set: &crate::pathspec::PathspecSet,
		initial_tracked: &std::collections::HashSet<String>,
		omitted: &mut Vec<String>,
		force: bool,
		excludes: &[DirIgnore],
		fold: bool,
	) -> Result<(), WorktreeError> {
		let base = pathspec.base_dir();
		// Tracked submodule (gitlink) mounts: the walker prunes these directories (opaque to `add`),
		// so it never descends into a submodule to stage its contents nor fails on an unreadable child.
		// Folded under `core.ignoreCase`; index-based `gitlink_mount` (a gitlink stage with no tracked
		// children) — a same-path blob-vs-gitlink conflict is still an opaque mount, as git treats it.
		let gitlinks: std::collections::HashSet<String> = index
			.entries
			.iter()
			.filter(|entry| gitlink_mount(index, &entry.path, fold))
			.map(|entry| {
				if fold {
					entry.path.to_ascii_lowercase()
				} else {
					entry.path.clone()
				}
			})
			.collect();
		let mut files = Vec::new();
		let mut stack = crate::checkout::ignore_prefix(self.work(), base, excludes)?;
		// Don't walk into an explicitly-named ignored base (`add 'ign/*'` with `.gitignore` containing
		// `ign/`): git stages no *untracked* file from it, so a glob whose only fresh candidates are ignored
		// is git's "did not match" unless a tracked entry also matched. `force` walks it (staging the ignored
		// content); a broad glob (empty base) is never a refused ignored base. The ignored-path advisory for
		// the base is recorded by the caller's `collect_ignored_advisory` pass. Probed vs git 2.50.1.
		let base_ignored =
			!base.is_empty() && !force && self.ignored_report_path(base, &stack, fold)?.is_some();
		// Walk only when the base directory exists and is not a refused ignored base — a glob under a missing
		// directory matches no present file, though it may still match a tracked deletion below. The seeded
		// `stack` carries the ancestor `.gitignore`s (root down to `base`'s parent) into the walk.
		if !base_ignored
			&& (base.is_empty() || matches!(self.work().lstat(base)?, Some(meta) if meta.kind.is_dir()))
		{
			walk_files(
				self.work(),
				base,
				&mut stack,
				&mut files,
				&gitlinks,
				force,
				fold,
			)?;
		}
		let mut matched = false;
		let mut staged: std::collections::HashSet<String> = std::collections::HashSet::new();
		// Track whether the walk recorded a *concrete* out-of-cone omission for this glob: git reports the
		// concrete path (`out/new`) rather than the glob text (`out/*`) when the glob swept up an untracked
		// out-of-cone file, and only falls back to the glob text when it matched a tracked skip-worktree
		// entry with no such concrete path (probed vs git 2.50.1).
		let omitted_before = omitted.len();
		for file in files {
			// git decides whether the glob matched *before* subtracting negatives (so `add '*.rs' :!*.rs`
			// is a no-op success); only the actual staging is gated by the exclusions.
			if pathspec.matches(&file) {
				matched = true;
				if !set.is_excluded(&file) {
					// An untracked out-of-cone file the glob swept up feeds the deferred sparse advice.
					self
						.stage_walked(index, &file, sparse, omitted, fold)
						.await?;
					staged.insert(file);
				}
			}
		}
		let walk_recorded_omission = omitted.len() > omitted_before;
		// A glob also "matched" if it hits a path tracked in the index *as it was before any staging*
		// (`initial_tracked` includes out-of-cone skip-worktree entries) — so a repeated glob still matches a
		// path an earlier spec removed (`add '*.rs' '*.rs'` on a deleted `a.rs`), and a tracked out-of-cone
		// entry counts. Track whether any such tracked match is genuinely out-of-cone (by the matcher, not
		// the skip-worktree bit — a bit-cleared dirty entry is still tracked) for the sparse advice below.
		// Accounting only; the actual staging works on the live index.
		let mut out_of_cone_tracked = false;
		for path in initial_tracked {
			if pathspec.matches(path) {
				matched = true;
				if sparse.is_some_and(|matcher| !matcher.includes(path)) {
					out_of_cone_tracked = true;
				}
			}
		}
		// Stage the IN-CONE tracked matches against the LIVE index — a present file/symlink the walk skipped
		// (a `.gitignore`d one) is restaged (git stages the modification), an absent one (or a file->directory
		// change) is a deletion. Unmerged (conflict) paths, which have no stage-0 entry, resolve the same way.
		// Snapshot first, since the removal loop mutates the index.
		let child = |path: &str| {
			(index
				.entry(path)
				.is_some_and(|entry| entry.stage == 0 && !entry.skip_worktree)
				|| index.unmerged_paths().any(|unmerged| unmerged == path))
				&& sparse.is_none_or(|matcher| matcher.includes(path))
				&& pathspec.matches(path)
		};
		let tracked: Vec<String> = index
			.entries
			.iter()
			.filter(|entry| entry.stage != 0 || !entry.skip_worktree)
			.map(|entry| entry.path.clone())
			.collect::<std::collections::BTreeSet<_>>()
			.into_iter()
			.filter(|path| child(path))
			.collect();
		for path in tracked {
			// Only the staging/removal is gated by the exclusions (so `add '*.rs' :!*.rs` on a deleted `a.rs`
			// is a no-op success).
			if set.is_excluded(&path) {
				continue;
			}
			match self.work().lstat(&path)? {
				Some(meta) if meta.kind.is_file() || meta.kind.is_symlink() => {
					if !staged.contains(&path) {
						self.stage_file(index, &path, sparse, fold).await?;
					}
				}
				// A tracked gitlink present as its mount directory is not a deletion — restage via `stage_file`
				// (updates the pointer to HEAD opaquely; resolves an unmerged gitlink to stage 0), never dropping it.
				Some(meta) if meta.kind.is_dir() && gitlink_mount(index, &path, fold) => {
					self.stage_file(index, &path, sparse, fold).await?;
				}
				// Absent, or now a directory (a file->directory change): the tracked entry is a deletion.
				_ => index.remove(&path),
			}
		}
		// An ignored named base whose only fresh candidates were ignored+untracked matched nothing here (the
		// walk was skipped), so `matched` is true only via a tracked entry — whose modification was staged by
		// the loop above (ignore never suppresses a tracked path). When nothing matched, it is git's "did not
		// match" (exit 128), which preempts the ignored advisory the caller collected for the base (probed vs
		// git 2.50.1: `add 'ign/*'` advises only when a tracked path matches; otherwise it did not match).
		if !matched {
			return Err(WorktreeError::PathspecMatch(spec.to_owned()));
		}
		// A glob rooted at a named directory (a non-empty literal base) EXPLICITLY targets that path, so a
		// tracked out-of-cone match there is the deferred sparse advice — reported BEFORE exclusions, so
		// `add 'out/*'` and `add 'out/*' :!out/*` both advise. A broad glob (empty base) silently skips a
		// tracked out-of-cone match it merely sweeps over (`add '*.rs'` matching only a tracked out-of-cone
		// file exits 0). Keying on the actual matches' cone state — not on whether the base *directory* tests
		// out-of-cone — is what keeps a non-cone glob that matches an included file from spuriously erroring.
		// (An untracked out-of-cone file swept by any glob is already recorded by `stage_walked`.) Probed vs
		// git 2.50.1.
		if !base.is_empty() && out_of_cone_tracked && !walk_recorded_omission {
			omitted.push(spec.to_owned());
		}
		Ok(())
	}

	/// Whether the directory pathspec `rel` matches paths but **all** of them are out-of-cone under
	/// active sparse-checkout — the case git refuses (reporting the pathspec) rather than staging
	/// nothing. `walked` are the on-disk files the pathspec's walk found (empty for a directory absent
	/// from the working tree). Returns `false` when sparse-checkout is inactive, when nothing is matched,
	/// or when any matched path is in-cone (a mixed directory stages its in-cone content and never errors,
	/// matching git). A matched path is a tracked entry at or under `rel` (in-cone iff not skip-worktree)
	/// or an on-disk file under `rel` — both in-cone iff the matcher includes them. A tracked entry is
	/// classified by the **matcher**, not its skip-worktree bit: a modified out-of-cone file that reapply
	/// left in place has its bit cleared yet is still outside the sparse definition (probed vs git
	/// 2.50.1 — git still refuses `add <dir>` there), so the bit would misclassify it as in-cone.
	fn only_out_of_cone_dir(
		&self,
		index: &Index<H>,
		rel: &str,
		walked: &[String],
		sparse: Option<&SparseCheckout>,
	) -> bool {
		let Some(matcher) = sparse else {
			return false;
		};
		let child_prefix = format!("{rel}/");
		let under = |path: &str| path == rel || path.starts_with(&child_prefix);
		let covered =
			!walked.is_empty() || index.entries.iter().any(|e| e.stage == 0 && under(&e.path));
		if !covered {
			return false;
		}
		let any_in_cone = index
			.entries
			.iter()
			.any(|e| e.stage == 0 && under(&e.path) && matcher.includes(&e.path))
			|| walked.iter().any(|f| matcher.includes(f));
		!any_in_cone
	}

	/// Stage a file discovered by a **walk** (a broad `.`/`<dir>`/glob traversal), recording it as a
	/// sparse omission first if git would advise about it. A walk silently skips a tracked skip-worktree
	/// entry that happens to be back on disk, but an *untracked* out-of-cone file it swept up is one git
	/// reports (probed vs git 2.50.1) — so only that case sets `omitted`. Staging itself is a no-op for any
	/// out-of-cone path.
	async fn stage_walked(
		&self,
		index: &mut Index<H>,
		path: &str,
		sparse: Option<&SparseCheckout>,
		omitted: &mut Vec<String>,
		fold: bool,
	) -> Result<(), WorktreeError> {
		// "Untracked" means *no index entry at all* — NOT merely a cleared skip-worktree bit. A modified
		// out-of-cone tracked file whose bit reapply cleared is still tracked, and a broad walk skips it
		// silently (probed vs git 2.50.1); only a genuinely untracked out-of-cone file is a reported
		// omission.
		if index.entry(path).is_none() && sparse.is_some_and(|matcher| !matcher.includes(path)) {
			// A *discovered* untracked out-of-cone path is reported once even if several pathspecs sweep it
			// up (`add . .` lists it once) — unlike an explicit tracked skip-worktree pathspec, which the
			// literal arms record per occurrence (`add out/a out/a` lists it twice). Probed vs git 2.50.1.
			push_unique(omitted, path.to_owned());
		}
		self.stage_file(index, path, sparse, fold).await
	}

	async fn stage_file(
		&self,
		index: &mut Index<H>,
		path: &str,
		sparse: Option<&SparseCheckout>,
		fold: bool,
	) -> Result<(), WorktreeError> {
		// A path outside the sparse-checkout is invisible to `add`: git refuses to update the index for a
		// path outside the sparse-checkout definition (advising `--sparse`), whether it already has a
		// skip-worktree entry OR is a newly-created out-of-cone file. Leave it untouched — neither restaging
		// a present-but-excluded file nor staging a new out-of-cone one.
		if index.is_sparse(path) || sparse.is_some_and(|matcher| !matcher.includes(path)) {
			return Ok(());
		}
		// A path INSIDE a tracked submodule mount is the submodule's to manage, not the superproject's:
		// `add` must never descend into a gitlink and stage its contents, which would replace the `160000`
		// gitlink with a `100644` subtree and silently corrupt the submodule on the next commit.
		if ancestor_is_gitlink(index, path, fold) {
			return Ok(());
		}
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
			// A tracked gitlink present as its mount directory: stage the submodule's current `HEAD`, so a
			// moved submodule records its new pointer on `add` the way git does — leaving it unchanged if the
			// submodule is not checked out / unresolvable. Any other directory at a slot is not staged.
			Some(meta) if meta.kind.is_dir() => {
				// Any stage (including an unmerged submodule's 1/2/3): `upsert` collapses them to a stage-0
				// gitlink at the submodule's current HEAD, resolving a submodule conflict the way `git add` does.
				if gitlink_mount(index, path, fold) {
					match crate::submodule_head_oid(self, path).await {
						Some(head) => {
							index.upsert(entry(path, 0o160000, head, &meta));
						}
						// An UNMERGED submodule with no checked-out HEAD cannot be resolved to a pointer — gta
						// errors rather than leaving the conflict stages silently in place. DEFERRED divergence:
						// for this narrow no-HEAD unmerged corner git's own broad-`add`/`add sub` behaviour is
						// content-dependent and inconsistent — an EMPTY mount dir errors "does not have a commit
						// checked out" (keeping `AA sub`), while a mount dir holding content drops the gitlink
						// stages and leaves `?? sub/` (probed vs git 2.55). gta uniformly errors here; matching
						// git's content-split needs recursing into the (absent) submodule and is left deferred.
						// A clean (stage-0) gitlink whose HEAD is merely unresolvable is left unchanged (as
						// `ls-files -m` treats it), not an error.
						None if index.unmerged_paths().any(|p| p == path) => {
							return Err(WorktreeError::SubmoduleNoCommit(path.to_owned()));
						}
						None => {}
					}
				}
			}
			Some(_) => {}
			None => index.remove(path),
		}
		Ok(())
	}

	/// Reconcile the index for tracked entries under `dir_rel` (the empty string is the work-tree root,
	/// matching everything) that the working-tree walk did not already stage (`walked`). The walk
	/// (`walk_files`) stages present, non-ignored files and prunes ignored directories with no knowledge
	/// of the index, so this pass covers what it misses, matching git 2.0+ `add <dir>` / `add .`:
	///
	/// - a tracked entry whose working-tree file **vanished** is staged as a deletion, so `rm foo && add
	///   .` records the removal rather than keeping the stale entry;
	/// - a tracked entry that is **present but under an ignored directory** — which the walk pruned — is
	///   restaged, since ignore rules never apply to an already-tracked path, so a modification to
	///   `ignored/tracked.rs` is picked up (probed vs git 2.50.1).
	///
	/// Entries the walk already staged are skipped, so `add .` does not re-hash every file twice. A
	/// sparse/out-of-cone or `:(exclude)`d entry is left untouched. A single explicit absent pathspec is
	/// handled directly by [`Self::add`]; this pass covers the directory case.
	async fn stage_tracked_outside_walk(
		&self,
		index: &mut Index<H>,
		dir_rel: &str,
		sparse: Option<&SparseCheckout>,
		set: &crate::pathspec::PathspecSet,
		walked: &std::collections::HashSet<String>,
		fold: bool,
	) -> Result<(), WorktreeError> {
		let prefix = if dir_rel.is_empty() {
			String::new()
		} else {
			format!("{dir_rel}/")
		};
		let admissible = |path: &str| {
			// The exact `dir_rel` is admissible too, not just its `dir_rel/` children: when a tracked *file*
			// `dir` is replaced by a directory, its stale file entry must be reconciled (staged as a
			// deletion) even though it does not lie under `dir/` (probed vs git 2.50.1).
			(path == dir_rel || path.starts_with(&prefix))
				&& sparse.is_none_or(|matcher| matcher.includes(path))
				&& !set.is_excluded(path)
				&& !walked.contains(path)
		};
		// Snapshot the candidate paths first, since the `lstat`/stage loop mutates the index.
		let mut candidates: Vec<String> = index
			.entries
			.iter()
			// An out-of-cone entry's file is absent by design — its absence is not a deletion. Exclude it by
			// the skip-worktree bit AND the active matcher: a dirty excluded file has its bit cleared (so it
			// stays visible), yet it is still outside the sparse definition and its later absence must not be
			// staged as a deletion (git leaves it unstaged). A `:(exclude)`d path is likewise left alone. A
			// path the walk already staged needs no second look.
			.filter(|entry| entry.stage == 0 && !entry.skip_worktree && admissible(&entry.path))
			.map(|entry| entry.path.clone())
			.collect();
		// An **unmerged** path (only stage 1/2/3 entries) is also a candidate: `add` resolves the conflict
		// by staging its present content, or — when its file is gone — recording the deletion, exactly as a
		// broad `add .` / `add :!x` does (probed vs git 2.50.1: `add :!nope` clears a deleted unmerged path's
		// higher-stage entries). Only reachable when the walk did not already stage it.
		candidates.extend(
			index
				.unmerged_paths()
				.filter(|path| admissible(path))
				.map(str::to_owned),
		);
		// DEFERRED divergence: this reconciliation `lstat`s the EXACT index spelling. Under `core.ignoreCase`
		// on a CASE-SENSITIVE filesystem, an indexed `Sub` whose on-disk mount is `sub` reports absent and the
		// deletion arm drops it. This is gitana's general recased-entry handling (a recased tracked FILE drops
		// the same way), not gitlink-specific; a fold-correct fix threads the folded on-disk spelling through
		// `stage_file` too. Left as a documented deferral (unreproducible on a case-insensitive host).
		for path in candidates {
			match self.work().lstat(&path)? {
				// Present as a file/symlink but unwalked — a tracked file the walk pruned as ignored. Restage
				// it (git stages modifications to tracked files regardless of ignore rules).
				Some(meta) if meta.kind.is_file() || meta.kind.is_symlink() => {
					self.stage_file(index, &path, sparse, fold).await?
				}
				// A tracked gitlink present as its mount DIRECTORY is not a deletion (the submodule is there):
				// restage via `stage_file`, which updates the pointer to the submodule HEAD opaquely. Without
				// this the mount dir would hit the deletion arm below and drop the gitlink, corrupting the tree.
				Some(meta) if meta.kind.is_dir() && gitlink_mount(index, &path, fold) => {
					self.stage_file(index, &path, sparse, fold).await?
				}
				// Gone from the working tree, or replaced by a directory (a tracked file `dir` now a `dir/`
				// tree): the stale file entry is a deletion.
				_ => index.remove(&path),
			}
		}
		Ok(())
	}

	/// Compute the three-way status: HEAD tree vs index (staged) and index vs
	/// working tree (unstaged), plus untracked files. `excludes_file` is the content of git's global
	/// excludes file (`core.excludesFile`), which the caller resolves because it lives outside the
	/// worktree; `None` when there is none. `core.ignoreCase` and `.git/info/exclude` are read
	/// internally (see [`crate::excludes`]).
	pub async fn status(&self, excludes_file: Option<&str>) -> Result<Status, WorktreeError> {
		crate::status::compute(self, excludes_file).await
	}

	/// git `ls-files`: list index and/or working-tree paths selected by `opts` and filtered by
	/// `pathspecs` (relative to `prefix`), rendered git's way (cwd-relative and C-quoted by default).
	/// `config` carries the values the caller resolves from git's full config stack (see
	/// [`LsFilesConfig`](crate::LsFilesConfig)). Returns the output text plus any unmatched-pathspec
	/// report.
	pub async fn ls_files(
		&self,
		pathspecs: &[&str],
		prefix: &str,
		opts: &crate::LsFilesOptions,
		config: &crate::LsFilesConfig<'_>,
	) -> Result<crate::LsFilesOutput, WorktreeError> {
		crate::ls_files::run(self, pathspecs, prefix, opts, config).await
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
	/// refuses to overwrite uncommitted local changes. Does not move `HEAD`. `excludes_file` is the
	/// content of git's global excludes file (`core.excludesFile`); the overwrite guard treats a file
	/// ignored by it (or by `.git/info/exclude`, or a `core.ignoreCase` case-variant) as expendable.
	/// `None` when there is none (internal callers, wasm).
	pub async fn checkout(
		&self,
		tree: ObjectId<H>,
		force: bool,
		excludes_file: Option<&str>,
	) -> Result<(), WorktreeError> {
		let mode = if force {
			crate::CheckoutMode::Reset
		} else {
			crate::CheckoutMode::Overlay
		};
		crate::checkout::run(self, tree, mode, excludes_file).await
	}

	/// Two-tree merge checkout (git's `read-tree -m -u`) from `head` to `target`: apply only the
	/// `head`→`target` diff, preserving non-conflicting local (staged or unstaged) changes and refusing
	/// conflicting ones — so staged work git would carry across a branch switch is not discarded. Backs
	/// `switch`. `head` is the tree the index currently matches (the branch being left).
	pub async fn checkout_merge(
		&self,
		head: ObjectId<H>,
		target: ObjectId<H>,
		excludes_file: Option<&str>,
	) -> Result<(), WorktreeError> {
		crate::checkout::run(
			self,
			target,
			crate::CheckoutMode::Merge { head },
			excludes_file,
		)
		.await
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
		crate::restore::run(
			self, source, worktree, staged, pathspecs, prefix, true, true,
		)
		.await
	}

	/// Reset the index to `tree`, replacing every entry with the tree's content (the index half
	/// of `git reset --mixed`). The working tree is left untouched, and `HEAD` is not moved.
	pub async fn reset_index(&self, tree: ObjectId<H>) -> Result<(), WorktreeError> {
		crate::reset::run(self, tree).await
	}

	/// Rebuild the index from `tree` **only if `.git/index` is missing**, atomically under the index lock —
	/// so a concurrent index writer cannot have its staged work discarded by the rebuild. A merge
	/// fast-forward, whose model assumes `index == HEAD`, uses this to repair a deleted/corrupt index before
	/// delegating to the two-tree merge, instead of taking that merge's rebuild-from-*target* fallback.
	pub async fn ensure_index_from_tree_if_missing(
		&self,
		tree: ObjectId<H>,
	) -> Result<(), WorktreeError> {
		crate::reset::ensure_from_tree_if_missing(self, tree).await
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
		crate::restore::run(
			self,
			Some(tree),
			false,
			true,
			pathspecs,
			prefix,
			false,
			false,
		)
		.await
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
		intent_to_add: false,
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
/// Turn `add`'s collected sparse omissions and ignored-path reports into its deferred result: `Ok(())`
/// when both are empty, else [`WorktreeError::PathspecAdvisory`] once the in-cone/tracked work has been
/// saved (git's exit-nonzero advice after a partial add). `sparse` keeps its argument/discovery order
/// (git lists out-of-cone pathspecs in the order encountered); `ignored` is sorted lexicographically and
/// de-duplicated (git lists ignored reports in byte order). Both probed vs git 2.50.1.
fn finish_advisory(sparse: Vec<String>, mut ignored: Vec<String>) -> Result<(), WorktreeError> {
	ignored.sort_unstable();
	ignored.dedup();
	if sparse.is_empty() && ignored.is_empty() {
		Ok(())
	} else {
		Err(WorktreeError::PathspecAdvisory { sparse, ignored })
	}
}

/// Append `value` to `list` unless already present, preserving first-seen order. Used for `add`'s
/// ignored-report accumulator (git lists each reported ignored path once). The sparse-omission
/// accumulator does NOT use this — git preserves duplicate out-of-cone pathspecs (`add out/a out/a`
/// lists `out/a` twice), so those push unconditionally. Probed vs git 2.50.1.
fn push_unique(list: &mut Vec<String>, value: String) {
	if !list.contains(&value) {
		list.push(value);
	}
}

/// The path an ignore probe uses for a glob pathspec, decoding backslash escapes: `\X` becomes a
/// literal `X`, so an escaped separator `dir\/foo` yields the decoded literal path `dir/foo` (whose
/// ignored ancestor `dir` git reports) rather than the empty root [`Pathspec::base_dir`] returns when it
/// treats `\` as a wildcard boundary. Scanning stops at the first *unescaped* `*`/`?`/`[`, after which
/// only the directory prefix (up to the last separator) is literal — matching git, which treats
/// `dir\/foo` as the literal path `dir/foo` for ignore purposes. Probed vs git 2.50.1.
fn glob_ignore_base(normalized: &str) -> String {
	let mut decoded = String::new();
	let mut last_sep = 0usize;
	let mut saw_wildcard = false;
	let mut chars = normalized.chars();
	while let Some(c) = chars.next() {
		match c {
			'\\' => match chars.next() {
				Some('/') => {
					decoded.push('/');
					last_sep = decoded.len();
				}
				Some(other) => decoded.push(other),
				None => {}
			},
			'*' | '?' | '[' => {
				saw_wildcard = true;
				break;
			}
			'/' => {
				decoded.push('/');
				last_sep = decoded.len();
			}
			other => decoded.push(other),
		}
	}
	// With a wildcard, only the directory prefix before it is literal; without one, the whole decoded
	// path is literal (`dir\/foo` → `dir/foo`, whose ancestor is still found by `ignored_report_path`).
	if saw_wildcard {
		decoded.truncate(last_sep.saturating_sub(1));
	}
	decoded
}

/// Collect the working-tree files under `dir_rel`. With `force` (git's `add -f`), ignored files and
/// directories are walked and staged too — `add -f .` / `add -f <dir>` / a forced glob stage the
/// ignored content, matching git 2.50.1; without it, ignored entries are pruned. `.git` is never
/// entered regardless.
/// Whether `path` is a submodule (gitlink) at ANY index stage (fold-aware) — including the stage 1/2/3
/// entries of an unmerged conflict. `add` must never descend into such a mount.
fn has_gitlink_stage<H: HashAlgorithm>(index: &Index<H>, path: &str, fold: bool) -> bool {
	let key = fold_case(path, fold);
	index
		.entries
		.iter()
		.any(|entry| fold_case(&entry.path, fold) == key && entry.mode == 0o160000)
}

/// Whether `path` has a tracked child `path/…` at any index stage (fold-aware): a mixed subtree
/// conflict where the on-disk directory holds tracked files, so `add` descends into it and stages
/// `path/new` rather than treating it as an opaque submodule mount.
fn has_tracked_child<H: HashAlgorithm>(index: &Index<H>, path: &str, fold: bool) -> bool {
	let prefix = format!("{}/", fold_case(path, fold));
	index
		.entries
		.iter()
		.any(|entry| fold_case(&entry.path, fold).starts_with(&prefix))
}

/// Whether `add` treats the directory at `path` as an opaque submodule mount — one to stage via `HEAD`
/// (or reject inside-paths for), never descending into. git decides this PURELY FROM THE INDEX and never
/// from the on-disk `.git` marker (probed vs git 2.55: a same-path blob-vs-gitlink conflict rejects
/// `add sub/new` identically whether `sub/` is a real checkout or a marker-free directory): the path has
/// a gitlink stage AND no tracked children. A same-path blob stage (a blob-vs-gitlink conflict) does NOT
/// make it non-opaque — git still treats the slot as a submodule boundary; only tracked `path/…` children
/// (a subtree-vs-gitlink conflict) turn it into a directory `add` descends into.
fn gitlink_mount<H: HashAlgorithm>(index: &Index<H>, path: &str, fold: bool) -> bool {
	has_gitlink_stage(index, path, fold) && !has_tracked_child(index, path, fold)
}

/// Case-fold `path` under `core.ignoreCase`, so an on-disk `sub` matches an indexed `Sub`.
fn fold_case(path: &str, fold: bool) -> String {
	if fold {
		path.to_ascii_lowercase()
	} else {
		path.to_owned()
	}
}

/// Whether any ancestor directory of `path` is a tracked submodule (gitlink) — i.e. `path` lies inside
/// a submodule mount. `add` treats such a mount as opaque, never staging the submodule's own contents
/// (which would replace the `160000` gitlink with an ordinary subtree). Fold-aware under
/// `core.ignoreCase`, so `add sub/f` finds an indexed `Sub`.
fn ancestor_is_gitlink<H: HashAlgorithm>(index: &Index<H>, path: &str, fold: bool) -> bool {
	gitlink_ancestor(index, path, fold).is_some()
}

/// The nearest ancestor of `path` that is a tracked submodule (gitlink), if any — the submodule that
/// `path` lies inside, in the INDEX's own casing (so the "is in submodule 'Sub'" error names it as git
/// does). Fold-aware under `core.ignoreCase`, so `add sub/f` finds an indexed `Sub`.
fn gitlink_ancestor<H: HashAlgorithm>(index: &Index<H>, path: &str, fold: bool) -> Option<String> {
	let mut rest = path;
	while let Some((parent, _)) = rest.rsplit_once('/') {
		if gitlink_mount(index, parent, fold) {
			let key = fold_case(parent, fold);
			return index
				.entries
				.iter()
				.find(|entry| entry.mode == 0o160000 && fold_case(&entry.path, fold) == key)
				.map(|entry| entry.path.clone())
				.or_else(|| Some(parent.to_owned()));
		}
		rest = parent;
	}
	None
}

fn walk_files<W: WorkDirFs>(
	work: &W,
	dir_rel: &str,
	stack: &mut Vec<DirIgnore>,
	out: &mut Vec<String>,
	gitlinks: &std::collections::HashSet<String>,
	force: bool,
	fold: bool,
) -> Result<(), WorktreeError> {
	// The gitlink set is folded under `core.ignoreCase` (see `add`); fold each lookup path the same way so
	// a case-variant on-disk mount still matches.
	let fold_key = |path: &str| {
		if fold {
			path.to_ascii_lowercase()
		} else {
			path.to_owned()
		}
	};
	// A walk ROOT that is itself a tracked gitlink mount (`gta add sub`, or a glob rooted at the mount)
	// is opaque — return before opening it, so a large submodule is not scanned and an unreadable child
	// cannot fail the add (git succeeds). Children are pruned in the loop below.
	if gitlinks.contains(&fold_key(dir_rel)) {
		return Ok(());
	}
	let pushed = push_gitignore(work, dir_rel, stack)?;
	for entry in work.read_dir(dir_rel)? {
		if entry.name == ".git" {
			continue;
		}
		let rel = join_rel(dir_rel, &entry.name);
		let is_dir = entry.kind.is_dir();
		if !force && ignore::is_ignored_fold(&rel, is_dir, stack, fold) {
			continue;
		}
		if is_dir {
			// A tracked submodule (gitlink) mount is opaque: do not descend into it — its contents are
			// the submodule's, never staged into the superproject (`stage_tracked_outside_walk` handles
			// the gitlink entry itself). Pruning here also avoids scanning a large submodule and failing
			// on an unreadable file inside it, matching git.
			if gitlinks.contains(&fold_key(&rel)) {
				continue;
			}
			walk_files(work, &rel, stack, out, gitlinks, force, fold)?;
		} else {
			out.push(rel);
		}
	}
	if pushed {
		stack.pop();
	}
	Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use std::sync::atomic::{AtomicU32, Ordering};

	use cap_std::ambient_authority;
	use cap_std::fs::Dir;
	use gitana_file_store_local::{CapWorkDir, LocalFileStore};
	use gitana_object::Sha256;
	use gitana_object_store::ObjectStore;
	use gitana_repository::Repository;

	use super::*;

	fn scratch(tag: &str) -> std::path::PathBuf {
		static SEQ: AtomicU32 = AtomicU32::new(0);
		let root = std::env::temp_dir().join(format!(
			"gitana-{tag}-{}-{}",
			std::process::id(),
			SEQ.fetch_add(1, Ordering::Relaxed)
		));
		std::fs::create_dir_all(root.join(".git")).unwrap();
		root
	}

	fn worktree(root: &std::path::Path) -> WorkTree<LocalFileStore, CapWorkDir, Sha256> {
		let git_dir = root.join(".git");
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&git_dir, ambient_authority()).unwrap(),
		)));
		WorkTree::new(
			repo,
			CapWorkDir::from_dir(Dir::open_ambient_dir(root, ambient_authority()).unwrap()),
			git_dir,
		)
	}

	/// Dropping a held `IndexLock` **before any worktree mutation** — a future cancelled between
	/// [`WorkTree::lock_index`] and the first write — must remove `index.lock` (the working tree still
	/// matches the index, so nothing is half-applied), so a later index write is not wedged by a stranded
	/// lock. Enforced at the guard, covering every operation that takes the lock.
	#[tokio::test]
	async fn dropping_a_held_index_lock_before_mutation_releases_it() {
		let root = scratch("lockdrop");
		let git_dir = root.join(".git");
		let wt = worktree(&root);

		{
			let _lock = wt.lock_index().await.unwrap();
			assert!(
				git_dir.join("index.lock").exists(),
				"the lock file is taken"
			);
			// `_lock` leaves scope here WITHOUT `commit_index`/`release_index_lock` — the cancellation path.
		}
		assert!(
			!git_dir.join("index.lock").exists(),
			"dropping the guard before mutation must remove index.lock, not strand it"
		);

		// It was truly released, not merely orphaned: the lock can be taken again.
		let lock = wt.lock_index().await.unwrap();
		wt.release_index_lock(lock).await;
		assert!(!git_dir.join("index.lock").exists());

		std::fs::remove_dir_all(&root).ok();
	}

	/// Once an operation has begun mutating the working tree, a cancellation (dropped guard) or an
	/// error release must **fail closed** — leave `index.lock` in place — so no later command proceeds
	/// against a half-applied working tree (the tree no longer matches the index). This is the
	/// `mark_mutation_started` half of the invariant in `docs/conventions.md`.
	#[tokio::test]
	async fn a_mid_mutation_drop_or_release_keeps_the_lock() {
		let root = scratch("lockstrand");
		let git_dir = root.join(".git");
		let wt = worktree(&root);

		// Cancellation after mutation began: the guard drops without commit/release, but the lock stays.
		{
			let lock = wt.lock_index().await.unwrap();
			lock.mark_mutation_started();
		}
		assert!(
			git_dir.join("index.lock").exists(),
			"a cancellation after mutation began must leave index.lock (fail-closed)"
		);
		std::fs::remove_file(git_dir.join("index.lock")).unwrap();

		// Error release after mutation began also keeps the lock (fail-closed), not removes it.
		let lock = wt.lock_index().await.unwrap();
		lock.mark_mutation_started();
		wt.release_index_lock(lock).await;
		assert!(
			git_dir.join("index.lock").exists(),
			"release after mutation began must leave index.lock (fail-closed)"
		);

		std::fs::remove_dir_all(&root).ok();
	}
}
