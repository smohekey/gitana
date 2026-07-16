//! A read-only working-tree status readout, wrapping `gitana-worktree`'s three-way `status`, tied to
//! the inspected destination so a stale result is never applied to a replaced path. A status
//! *computation* that fails is a [`LinkedWorktreeError`] — never silently reported as clean.

use std::path::PathBuf;

use gitana_worktree::{Status, StatusEntry};

/// The status of one linked worktree's working tree, associated with its destination identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeStatusReport {
	/// The destination this status was computed for.
	pub destination: PathBuf,
	status: Status,
}

/// git's unmerged (conflict) index/worktree code pairs.
const CONFLICT_PAIRS: &[(char, char)] = &[
	('D', 'D'),
	('A', 'U'),
	('U', 'D'),
	('U', 'A'),
	('D', 'U'),
	('A', 'A'),
	('U', 'U'),
];

impl WorktreeStatusReport {
	/// The underlying three-way status (tracked changes with their `X`/`Y` codes, and untracked paths).
	pub fn status(&self) -> &Status {
		&self.status
	}

	/// Whether the working tree is clean — no tracked changes and no untracked paths.
	pub fn is_clean(&self) -> bool {
		self.status.changed.is_empty() && self.status.untracked.is_empty()
	}

	/// Whether any **tracked** path has a change (staged, unstaged, conflicted, or missing) — the tracked-side
	/// of dirtiness, independent of untracked-path detection. Removal gates on this (plus a separate
	/// matcher-independent residual scan for untracked/ignored content), so a case- or ignore-matcher
	/// false-positive in the untracked list can never make a clean worktree look dirty here.
	pub fn has_tracked_changes(&self) -> bool {
		!self.status.changed.is_empty()
	}

	/// `git status --porcelain=v1` rendering.
	pub fn porcelain_v1(&self) -> String {
		self.status.porcelain_v1()
	}

	/// Whether any path has staged (index-vs-HEAD) changes.
	pub fn has_staged(&self) -> bool {
		self
			.status
			.changed
			.iter()
			.any(|e| !self.is_conflict(e) && e.index != ' ' && e.index != '?')
	}

	/// Whether any tracked path has unstaged (worktree-vs-index) modifications.
	pub fn has_unstaged(&self) -> bool {
		self
			.status
			.changed
			.iter()
			.any(|e| !self.is_conflict(e) && e.worktree == 'M')
	}

	/// Whether there are any untracked paths.
	pub fn has_untracked(&self) -> bool {
		!self.status.untracked.is_empty()
	}

	/// Whether any path is in an unmerged (conflicted) state.
	pub fn has_conflicts(&self) -> bool {
		self.status.changed.iter().any(|e| self.is_conflict(e))
	}

	/// Whether any tracked path is missing from the working tree (deleted, not staged as a deletion).
	pub fn has_missing(&self) -> bool {
		self
			.status
			.changed
			.iter()
			.any(|e| !self.is_conflict(e) && e.worktree == 'D')
	}

	fn is_conflict(&self, entry: &StatusEntry) -> bool {
		CONFLICT_PAIRS.contains(&(entry.index, entry.worktree))
	}
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use super::*;

	use std::path::Path;

	use gitana_object::{HashKind, ObjectId, Sha1, Sha256};
	use gitana_object_store::ObjectStore;
	use gitana_repository::Repository;
	use gitana_worktree::WorkTree;

	use crate::WorktreeObjectId;

	use crate::pointers::{
		admin_dirs_for, canonical_eq, checkout_gitfile_names, is_bare, is_leaf_symlink,
		main_checkout_identifies_common,
	};
	use crate::repo_id::{detect_kind, open_store_raw, open_work_dir};
	use crate::{LinkedWorktreeError, RepositoryId};

	/// Compute the working-tree status of the worktree at `destination` in repository `repo`. The
	/// destination must be a worktree of `repo` (its main worktree or a registered linked worktree);
	/// otherwise this is a hard error (a status cannot be attributed to a non-worktree path).
	pub async fn status(
		repo: &RepositoryId,
		destination: &Path,
	) -> Result<WorktreeStatusReport, LinkedWorktreeError> {
		if !destination.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(destination.to_path_buf()));
		}
		// A destination that is itself a symlink is never a worktree we status — following the alias to open
		// its target's `.git`/index would violate the no-follow boundary. It is a hard error, not a report.
		if is_leaf_symlink(destination) {
			return Err(LinkedWorktreeError::io(
				"status: destination is a symlink, not a worktree",
				destination,
				std::io::Error::from(std::io::ErrorKind::InvalidInput),
			));
		}
		let common = repo.common_dir();
		// A registered linked worktree — accepted only when its checkout is *live* (the checkout's `.git`
		// gitfile names the admin). A stale registration whose path was reused is not a worktree we can
		// status; it falls through to the hard error rather than opening an unrelated directory with the
		// stale admin index.
		// A single live registration is a linked worktree we can status. Zero, a duplicate (corruption), or
		// a stale registration (its checkout gone/reused) all fall through to the hard error below.
		let registered = match admin_dirs_for(common, destination)?.as_slice() {
			[admin] if checkout_gitfile_names(destination, admin)? => Some(admin.clone()),
			_ => None,
		};
		// The destination is the *main* worktree when its `.git` currently identifies `common` — an
		// ordinary main worktree's `.git` *is* `common` (a directory); a `--separate-git-dir` main
		// worktree's `.git` is a gitfile pointing at the external `common`. This identity check is the
		// authoritative test (it does not depend on how the `RepositoryId` was obtained — explicit
		// `at_common_dir`, discovery from the primary, or discovery from a linked worktree — and it closes
		// the replaced-checkout hole, since a moved/replaced separate-git-dir checkout no longer names
		// `common`). A bare repository has no main working tree, and the common dir itself is never one.
		let is_main = !is_bare(common)?
			&& !canonical_eq(destination, common)
			&& main_checkout_identifies_common(destination, common)?;
		// The per-worktree git dir holding this destination's index/HEAD.
		let git_dir = if let Some(admin) = registered {
			admin
		} else if is_main {
			common.to_path_buf()
		} else {
			return Err(LinkedWorktreeError::io(
				"status: not a worktree of this repository",
				destination,
				std::io::Error::from(std::io::ErrorKind::NotFound),
			));
		};

		status_at(&git_dir, common, destination).await
	}

	/// Compute the working-tree status of the checkout at `destination`, opened against the per-worktree
	/// `git_dir` (the admin dir for a linked worktree, or the common dir for the main worktree) and the shared
	/// `common`. The caller has already established that `destination` is a live worktree of the repository;
	/// this only opens the stores, detects the format, and runs the three-way status. Shared by the public
	/// [`status`] entry point and [`inspect`](crate::inspect) (when a status is requested for a cleanup
	/// decision), so both attribute a status to a worktree identically.
	pub(crate) async fn status_at(
		git_dir: &Path,
		common: &Path,
		destination: &Path,
	) -> Result<WorktreeStatusReport, LinkedWorktreeError> {
		let store = open_store_raw(git_dir, common)?;
		let work = open_work_dir(destination)?;
		let status = match detect_kind(&store).await? {
			HashKind::Sha1 => {
				let repo = Repository::<_, Sha1>::new(ObjectStore::new(store));
				WorkTree::new(repo, work, git_dir.to_path_buf())
					.status()
					.await?
			}
			HashKind::Sha256 => {
				let repo = Repository::<_, Sha256>::new(ObjectStore::new(store));
				WorkTree::new(repo, work, git_dir.to_path_buf())
					.status()
					.await?
			}
		};
		Ok(WorktreeStatusReport {
			destination: destination.to_path_buf(),
			status,
		})
	}

	/// The stage-0 tracked paths at `destination` present on disk whose content or mode diverges from the index,
	/// verified by **always hashing** the working file (never the index stat cache). Opened the same way as
	/// [`status_at`]. Catches edits `status` can miss — a stat-preserving/same-size rewrite, a coarse-timestamp
	/// filesystem — and skip-worktree edits `status` omits entirely; safe removal must preserve any it returns.
	/// An empty result means every present tracked file hashes equal to the index (reconstructable, safe to
	/// delete). See
	/// [`WorkTree::diverged_tracked_content_paths`](gitana_worktree::WorkTree::diverged_tracked_content_paths).
	pub(crate) async fn diverged_tracked_content_paths(
		git_dir: &Path,
		common: &Path,
		destination: &Path,
	) -> Result<Vec<String>, LinkedWorktreeError> {
		let store = open_store_raw(git_dir, common)?;
		let work = open_work_dir(destination)?;
		let paths = match detect_kind(&store).await? {
			HashKind::Sha1 => {
				WorkTree::new(
					Repository::<_, Sha1>::new(ObjectStore::new(store)),
					work,
					git_dir.to_path_buf(),
				)
				.diverged_tracked_content_paths()
				.await?
			}
			HashKind::Sha256 => {
				WorkTree::new(
					Repository::<_, Sha256>::new(ObjectStore::new(store)),
					work,
					git_dir.to_path_buf(),
				)
				.diverged_tracked_content_paths()
				.await?
			}
		};
		Ok(paths)
	}

	/// Whether the worktree at `destination` uses a **sparse index** (`git sparse-checkout --sparse-index`) —
	/// its index carries a collapsed `040000` sparse-directory entry that gitana does not expand. Opened the
	/// same way as [`status_at`] but only the index is read (no working-tree walk). Safe removal uses this to
	/// refuse honestly (a clear "sparse-index unsupported" signal) rather than acting on the spurious add/delete
	/// pairs `status` would otherwise report for such a checkout. See
	/// [`WorkTree::is_sparse_index`](gitana_worktree::WorkTree::is_sparse_index).
	pub(crate) async fn is_sparse_index(
		git_dir: &Path,
		common: &Path,
		destination: &Path,
	) -> Result<bool, LinkedWorktreeError> {
		let store = open_store_raw(git_dir, common)?;
		let work = open_work_dir(destination)?;
		let sparse = match detect_kind(&store).await? {
			HashKind::Sha1 => {
				WorkTree::new(
					Repository::<_, Sha1>::new(ObjectStore::new(store)),
					work,
					git_dir.to_path_buf(),
				)
				.is_sparse_index()
				.await?
			}
			HashKind::Sha256 => {
				WorkTree::new(
					Repository::<_, Sha256>::new(ObjectStore::new(store)),
					work,
					git_dir.to_path_buf(),
				)
				.is_sparse_index()
				.await?
			}
		};
		Ok(sparse)
	}

	/// Whether the checkout-missing partial at `git_dir` has staged (index-vs-`HEAD`) or unmerged changes in its
	/// retained index — computed **without a working tree** (the checkout is gone). Cleaning such a partial drops
	/// the admin dir and its index, erasing staged state and orphaning index-only blobs, so safe removal refuses
	/// when this is true. Opened over the routing store (the index and `HEAD` live per-worktree in `git_dir`, the
	/// objects in `common`); the unused work capability is rooted at `git_dir`, which exists.
	pub(crate) async fn partial_has_staged_changes(
		git_dir: &Path,
		common: &Path,
	) -> Result<bool, LinkedWorktreeError> {
		let store = open_store_raw(git_dir, common)?;
		let work = open_work_dir(git_dir)?; // never read by `has_staged_changes`; the checkout is absent
		let staged = match detect_kind(&store).await? {
			HashKind::Sha1 => {
				WorkTree::new(
					Repository::<_, Sha1>::new(ObjectStore::new(store)),
					work,
					git_dir.to_path_buf(),
				)
				.has_staged_changes()
				.await?
			}
			HashKind::Sha256 => {
				WorkTree::new(
					Repository::<_, Sha256>::new(ObjectStore::new(store)),
					work,
					git_dir.to_path_buf(),
				)
				.has_staged_changes()
				.await?
			}
		};
		Ok(staged)
	}

	/// Whether the worktree's `HEAD` commit `head` is reachable from any **shared** ref (anything under `refs/`
	/// in the common dir — `refs/heads`, `refs/tags` peeled through annotated tags, `refs/remotes`, …). Removing
	/// a worktree drops its admin dir, and with it the worktree's *own* `HEAD` and per-worktree refs
	/// (`refs/worktree|bisect|rewritten/*`) — the only references a **detached** HEAD, or a HEAD symbolic to a
	/// per-worktree ref, has to its commit. Such a commit reachable from no shared ref would be orphaned (later
	/// gc-able), so safe removal refuses. A HEAD symbolic to a shared branch (`refs/heads/*`) is *not* routed
	/// here (its branch survives); the caller short-circuits that.
	///
	/// Opened over the **common** store specifically — where only the shared refs and objects live — so this
	/// worktree's own per-worktree refs (physically in its admin dir) are correctly excluded, never counted as a
	/// surviving anchor. Reachability from *another* worktree's detached HEAD is likewise not consulted: omitting
	/// it can only *over*-refuse (a safe refusal), never authorise deleting an orphan.
	/// The first **admin-local anchor commit** that removal would orphan — reachable from no shared ref — or
	/// `None` if every such commit is preserved. Removing a worktree deletes its admin dir, and with it every
	/// reference that lives there: a **detached** (or per-worktree-symbolic) `HEAD`, and the whole per-worktree
	/// ref namespaces `refs/worktree/*`, `refs/bisect/*`, `refs/rewritten/*`. Each of those tips uniquely anchors
	/// its commit; if that commit is reachable from no *surviving* shared ref, dropping the admin orphans it
	/// (later gc-able), so safe removal must refuse. `head` is the `HEAD` commit passed **only when its own
	/// anchor will not survive** (the caller short-circuits a `refs/heads/*` HEAD, whose branch survives); the
	/// per-worktree ref tips are always checked.
	///
	/// Reachability is judged over the **common** store (shared refs + objects only, so this worktree's own
	/// per-worktree refs are never miscounted as a surviving anchor); the per-worktree ref *tips* are read from
	/// the admin (`git_dir`) via the routing store. Returns the first unreachable anchor so a caller can name the
	/// commit to preserve.
	pub(crate) async fn first_unreachable_admin_anchor(
		git_dir: &Path,
		common: &Path,
		head: Option<&WorktreeObjectId>,
	) -> Result<Option<WorktreeObjectId>, LinkedWorktreeError> {
		let common_store = open_store_raw(common, common)?;
		match detect_kind(&common_store).await? {
			HashKind::Sha1 => unreachable_anchor::<Sha1>(git_dir, common, head).await,
			HashKind::Sha256 => unreachable_anchor::<Sha256>(git_dir, common, head).await,
		}
	}

	/// The per-worktree ref namespaces that live inside the admin dir and so are deleted with it — git's
	/// `is_per_worktree_ref` set.
	const PER_WORKTREE_REF_PREFIXES: [&str; 3] =
		["refs/worktree/", "refs/bisect/", "refs/rewritten/"];

	async fn unreachable_anchor<H: gitana_object::HashAlgorithm>(
		git_dir: &Path,
		common: &Path,
		head: Option<&WorktreeObjectId>,
	) -> Result<Option<WorktreeObjectId>, LinkedWorktreeError>
	where
		ObjectId<H>: crate::object_id::IntoWorktreeObjectId,
	{
		use crate::object_id::IntoWorktreeObjectId;
		// Reachability roots (shared refs + objects) come from a store rooted wholly at `common`; the admin's own
		// per-worktree refs are read from a routing store rooted at `git_dir`.
		let shared = Repository::<_, H>::new(ObjectStore::new(open_store_raw(common, common)?));
		let admin = Repository::<_, H>::new(ObjectStore::new(open_store_raw(git_dir, common)?));

		// The anchor commits that die with the admin: the (unanchored) HEAD commit, then every per-worktree tip.
		let mut anchors: Vec<ObjectId<H>> = Vec::new();
		if let Some(h) = head {
			anchors.push(ObjectId::<H>::from_hex(&h.to_hex()).map_err(|_| {
				LinkedWorktreeError::InvalidObjectId {
					kind: h.kind(),
					hex: h.to_hex(),
				}
			})?);
		}
		for prefix in PER_WORKTREE_REF_PREFIXES {
			// Direct tips (`list`) **and** the objects that *symbolic* per-worktree refs resolve to (`list` skips
			// symbolic refs, so `refs/worktree/save -> ORIG_HEAD` anchoring an otherwise-unreachable commit would
			// otherwise be missed and the commit orphaned) — resolved through the admin store, where the
			// per-worktree targets live.
			for (_name, oid) in admin.refs().list(prefix).await? {
				anchors.push(oid);
			}
			for oid in admin.refs().symbolic_ref_targets(prefix).await? {
				anchors.push(oid);
			}
		}

		for anchor in anchors {
			if !reachable_from_shared_refs(&shared, anchor).await? {
				return Ok(Some(anchor.tag()));
			}
		}
		Ok(None)
	}

	/// Whether `target` is reachable from (an ancestor of, or equal to) any ref under `refs/` — each ref peeled
	/// through annotated tags to its commit; a ref that does not peel to a commit is skipped. Early-exits on the
	/// first reaching tip.
	async fn reachable_from_shared_refs<
		F: gitana_file_store::FileStore,
		H: gitana_object::HashAlgorithm,
	>(
		repo: &Repository<F, H>,
		target: ObjectId<H>,
	) -> Result<bool, LinkedWorktreeError> {
		// Direct refs under `refs/` (`list` returns these) **and** the objects that *symbolic* refs under
		// `refs/` resolve to (`list` intentionally skips symbolic refs, so a commit anchored only through one —
		// e.g. `refs/tags/anchor -> CUSTOM1` — would otherwise be missed and the removal spuriously refused).
		let direct = repo
			.refs()
			.list("refs/")
			.await?
			.into_iter()
			.map(|(_, oid)| oid);
		let symbolic = repo.refs().symbolic_ref_targets("refs/").await?.into_iter();
		for oid in direct.chain(symbolic) {
			// A ref that does not peel to a commit (a ref-to-blob/tree, or a broken tag) cannot preserve the
			// commit, so skip it rather than fail the whole removal.
			let Ok(tip) = repo.peel_to_commit(oid).await else {
				continue;
			};
			if repo.is_ancestor(target, tip).await? {
				return Ok(true);
			}
		}
		Ok(false)
	}

	/// The maximum number of residual (untracked/ignored) paths [`residual_untracked_paths`] collects — enough
	/// to explain a refusal without building an unbounded list; the walk stops once this many are found (the
	/// removal decision needs only *whether* any exist).
	const MAX_RESIDUAL_SAMPLE: usize = 64;

	/// The working-tree files at `destination` that are **not tracked** in the worktree's index — the residual
	/// (untracked *or* ignored) content that safe removal must preserve rather than recursively delete. This is
	/// a **matcher-independent** scan: it never consults `.gitignore`, so a non-git-faithful ignore
	/// false-positive can never let a git-*untracked* file pass as removable. An empty result means the working
	/// tree contains solely tracked files (pristine). Tracked-path membership is **exact** (byte-for-byte,
	/// aside from a Windows-only `\`→`/` separator normalisation — on Unix `\` is a valid filename byte): case
	/// is deliberately **not** folded. Folding is unsound here — `core.ignorecase` can be set on a
	/// case-sensitive volume, and per-directory case folding (ext4/F2FS) defeats any whole-tree probe — so a
	/// fold could let a genuinely case-distinct *untracked* file (`FOO` vs tracked `foo`) pass as tracked and be
	/// deleted. Exact matching is fully safe; its only cost is over-refusing an mv-based case-only rename on a
	/// case-insensitive filesystem (a rare, *safe* refusal). The list is capped at [`MAX_RESIDUAL_SAMPLE`]; a
	/// non-UTF-8 filename can hold no index entry, so it counts as residual.
	pub(crate) async fn residual_untracked_paths(
		git_dir: &Path,
		common: &Path,
		destination: &Path,
	) -> Result<Vec<String>, LinkedWorktreeError> {
		let store = open_store_raw(git_dir, common)?;
		let work = open_work_dir(destination)?;
		// The set of tracked paths, normalised the same way the disk walk normalises each candidate, so
		// membership is an exact O(1) lookup.
		let tracked: std::collections::HashSet<String> = match detect_kind(&store).await? {
			HashKind::Sha1 => {
				let wt = WorkTree::new(
					Repository::<_, Sha1>::new(ObjectStore::new(store)),
					work,
					git_dir.to_path_buf(),
				);
				wt.load_index()
					.await?
					.entries
					.iter()
					.map(|e| normalize_index_path(&e.path))
					.collect()
			}
			HashKind::Sha256 => {
				let wt = WorkTree::new(
					Repository::<_, Sha256>::new(ObjectStore::new(store)),
					work,
					git_dir.to_path_buf(),
				);
				wt.load_index()
					.await?
					.entries
					.iter()
					.map(|e| normalize_index_path(&e.path))
					.collect()
			}
		};
		let mut residual = Vec::new();
		collect_residual(destination, destination, &tracked, &mut residual)?;
		Ok(residual)
	}

	/// Normalise a path for exact tracked-set membership. Separators are converted to `/` **only on Windows**
	/// (where the OS uses `\`): on Unix `\` is a *valid filename byte*, and both index keys and the on-disk
	/// relative path already use `/`, so converting it there would be non-injective (`a\b` and `a/b` would
	/// collide, letting an untracked file masquerade as a tracked one). Case is **not** folded (see
	/// [`residual_untracked_paths`]).
	fn normalize_index_path(path: &str) -> String {
		#[cfg(windows)]
		{
			path.replace('\\', "/")
		}
		#[cfg(not(windows))]
		{
			path.to_owned()
		}
	}

	/// The filesystem's **actual stored name** for the checkout's root `.git` pointer — `canonicalize(root/.git)`'s
	/// leaf, or the literal `.git` if that fails. On a case-insensitive filesystem a `.GIT`-spelled pointer
	/// canonicalizes back to `.GIT` (the real entry), so the residual scan skips exactly that entry; a hard link
	/// under a *different* name — `gitfile-backup`, or a case-sensitive filesystem's distinct `.GIT` (whose name
	/// differs from the gitfile's canonical `.git`) — does not match and is preserved as residual. Matching the
	/// *name* (not merely the inode) is what distinguishes a case alias from a second hard-link entry.
	fn gitfile_entry_name(root: &Path) -> std::ffi::OsString {
		std::fs::canonicalize(root.join(".git"))
			.ok()
			.and_then(|p| p.file_name().map(|n| n.to_owned()))
			.unwrap_or_else(|| std::ffi::OsString::from(".git"))
	}

	/// Recursively collect the files under `dir` (relative to the worktree `root`) that are **not** in
	/// `tracked` (normalised the same way). The root-level `.git` pointer is skipped. Stops once
	/// [`MAX_RESIDUAL_SAMPLE`] residual paths are found. A directory read failure is a hard error (never
	/// silently "tracked"); a non-UTF-8 relative path is residual (it can hold no index entry).
	fn collect_residual(
		root: &Path,
		dir: &Path,
		tracked: &std::collections::HashSet<String>,
		out: &mut Vec<String>,
	) -> Result<(), LinkedWorktreeError> {
		// Only the root level holds the checkout's own `.git` pointer; resolve its real stored name once here.
		let git_name = (dir == root).then(|| gitfile_entry_name(root));
		for entry in
			std::fs::read_dir(dir).map_err(|e| LinkedWorktreeError::io("scanning worktree", dir, e))?
		{
			if out.len() >= MAX_RESIDUAL_SAMPLE {
				return Ok(());
			}
			let entry = entry.map_err(|e| LinkedWorktreeError::io("scanning worktree", dir, e))?;
			let path = entry.path();
			// Skip the worktree's own `.git` pointer at the root — matched by its real stored *name*
			// ([`gitfile_entry_name`]), not by inode: an untracked hard link to the gitfile under another name
			// (`gitfile-backup`, or a case-sensitive filesystem's distinct `.GIT`) shares the inode but is a
			// genuine untracked entry the scan must preserve (removal trusts this scan over `status`'s untracked
			// list), while a case-insensitive filesystem's `.GIT`-spelled pointer *is* the one gitfile entry.
			if let Some(git_name) = &git_name
				&& entry.file_name() == *git_name
			{
				continue;
			}
			let meta = std::fs::symlink_metadata(&path)
				.map_err(|e| LinkedWorktreeError::io("scanning worktree", &path, e))?;
			if meta.is_dir() {
				collect_residual(root, &path, tracked, out)?;
			} else {
				// A file or symlink is residual unless its normalised `/`-relative path is tracked exactly.
				let rel = path.strip_prefix(root).ok().and_then(|r| r.to_str());
				match rel {
					Some(rel) => {
						let key = normalize_index_path(rel);
						if !tracked.contains(&key) {
							// Report the worktree-relative path with `/` separators (a no-op on Unix, where the
							// path is already `/`-joined and `\` is a literal filename byte).
							#[cfg(windows)]
							out.push(rel.replace('\\', "/"));
							#[cfg(not(windows))]
							out.push(rel.to_owned());
						}
					}
					// A non-UTF-8 relative path: keep it worktree-*relative* (strip the root) and lossily convert,
					// so `ResidualContent.paths` stays relative as documented.
					None => out.push(
						path
							.strip_prefix(root)
							.unwrap_or(&path)
							.to_string_lossy()
							.into_owned(),
					),
				}
			}
		}
		Ok(())
	}

	#[cfg(test)]
	mod tests {
		use super::{gitfile_entry_name, normalize_index_path};

		#[test]
		fn normalizes_platform_separators_without_folding_case() {
			// Case is preserved (never folded — folding is unsound for the residual gate).
			assert_eq!(normalize_index_path("Foo/Bar"), "Foo/Bar");
			// `\` is a separator only on Windows; on Unix it is a valid filename byte and must be preserved
			// (otherwise `a\b` and `a/b` would collide and an untracked file could masquerade as tracked).
			#[cfg(windows)]
			assert_eq!(normalize_index_path("a\\b\\c"), "a/b/c");
			#[cfg(not(windows))]
			assert_eq!(normalize_index_path("a\\b\\c"), "a\\b\\c");
		}

		#[cfg(unix)]
		#[test]
		fn gitfile_entry_name_is_the_pointers_real_name_not_a_hardlink() {
			use std::sync::atomic::{AtomicU32, Ordering};
			static SEQ: AtomicU32 = AtomicU32::new(0);
			let dir = std::env::temp_dir().join(format!(
				"gitana-gitname-{}-{}",
				std::process::id(),
				SEQ.fetch_add(1, Ordering::Relaxed)
			));
			std::fs::create_dir_all(&dir).unwrap();
			std::fs::write(dir.join(".git"), b"gitdir: /x\n").unwrap();

			// The gitfile's stored name is `.git`. A hard link under another name shares the inode but the
			// resolved *name* stays `.git` — so the residual scan skips only `.git`, never the hard link (which is
			// preserved as residual). This is the property that name-matching (not inode identity) guarantees.
			assert_eq!(gitfile_entry_name(&dir), std::ffi::OsStr::new(".git"));
			std::fs::hard_link(dir.join(".git"), dir.join("gitfile-backup")).unwrap();
			assert_eq!(
				gitfile_entry_name(&dir),
				std::ffi::OsStr::new(".git"),
				"a hard link must not change the gitfile's resolved name"
			);

			let _ = std::fs::remove_dir_all(&dir);
		}
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::status;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{
	diverged_tracked_content_paths, first_unreachable_admin_anchor, is_sparse_index,
	partial_has_staged_changes, residual_untracked_paths, status_at,
};
