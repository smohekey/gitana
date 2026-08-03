//! git's "standard excludes" for untracked / ignored detection, resolved for one working tree.
//!
//! Everywhere git decides whether a working-tree path is untracked-and-not-ignored — `status`, `add`,
//! `checkout`'s overwrite guard — it consults three exclude sources and folds them under
//! `core.ignoreCase`: per-directory `.gitignore` (highest priority), then `.git/info/exclude`, then the
//! global `core.excludesFile` (lowest). The [`ignore`](crate::ignore) matcher is source-agnostic and
//! fold-capable; this module supplies the two *whole-tree* levels below per-directory `.gitignore` and
//! the fold flag, so each caller seeds its stack with [`StandardExcludes::base`], pushes per-directory
//! `.gitignore` on top during its walk, and matches with
//! [`ignore::is_ignored_fold`](crate::ignore::is_ignored_fold) using [`StandardExcludes::fold`].
//!
//! `core.ignoreCase` and `.git/info/exclude` are read here (both reachable inside the sandboxed
//! worktree crate — the effective config, and the `.git` file store). `core.excludesFile` lives
//! *outside* the worktree, so the caller resolves and passes its content (`None` when there is none, as
//! for the wasm component and internal porcelain callers). This mirrors `ls-files`
//! (`--exclude-standard`), which already consults the same three sources.

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::HashAlgorithm;

use crate::ignore::{self, DirIgnore};
use crate::{WorkTree, WorktreeError};

/// The resolved standard-excludes context for one working tree: the case-fold flag and the whole-tree
/// exclude levels that sit below per-directory `.gitignore`.
pub(crate) struct StandardExcludes {
	/// `core.ignoreCase` — fold pattern matching (and the caller's tracked-membership checks) to ASCII
	/// lower case. Read from the effective config, exactly as sparse-checkout matching reads it.
	pub fold: bool,
	/// The exclude levels below per-directory `.gitignore`, lowest priority first: the global
	/// `core.excludesFile` (if any), then `.git/info/exclude`. A caller clones this to seed a fresh
	/// per-directory stack, or moves it in as the initial stack for a single walk.
	pub base: Vec<DirIgnore>,
}

/// Resolve [`StandardExcludes`] for `wt`. `excludes_file` is the content of git's global excludes file
/// (`core.excludesFile`, else the XDG default), which the caller reads because it lives outside the
/// worktree sandbox; `None` when there is none.
pub(crate) async fn standard_excludes<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	excludes_file: Option<&str>,
) -> Result<StandardExcludes, WorktreeError> {
	Ok(StandardExcludes {
		fold: ignore_case(wt).await?,
		base: load_base(wt, excludes_file).await?,
	})
}

/// The whole-tree exclude levels alone — the global `core.excludesFile` (caller-supplied content) then
/// `.git/info/exclude` — without resolving `core.ignoreCase`. `checkout` resolves the fold flag
/// separately (it needs it on the force path too, where the exclude *files* are not read), so it loads
/// the base only for its non-force overwrite guard.
pub(crate) async fn load_base<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	excludes_file: Option<&str>,
) -> Result<Vec<DirIgnore>, WorktreeError> {
	// Lowest priority first, so a later per-directory `.gitignore` (pushed on top by the caller)
	// overrides them — git's last-match-wins precedence over the stack.
	let mut base = Vec::new();
	if let Some(text) = excludes_file {
		base.push(ignore::parse(text, ""));
	}
	if let Some(text) = read_info_exclude(wt).await? {
		base.push(ignore::parse(&text, ""));
	}
	Ok(base)
}

/// Resolve `core.ignoreCase` without loading any exclude file — the fold flag alone. `git add -f`
/// bypasses exclude-file processing (a directory `.git/info/exclude` is not fatal there) but *still*
/// validates `core.ignoreCase` (a malformed value aborts, probed vs git 2.55), so the forced-add path
/// uses this rather than [`standard_excludes`].
///
/// When the frontend has installed a merged effective config (native `gta`), it already resolves git's
/// full precedence stack — system < global < local < `config.worktree` — so it is authoritative and read
/// directly. Only for a **bare** `Repository` with no merged view (the linked-worktree removal-safety
/// status, the wasm component) does the raw-local fallback miss the per-worktree override: git honours a
/// `core.ignoreCase` set in `config.worktree` when `extensions.worktreeConfig` is on (probed vs git
/// 2.55), and missing it could fold-hide an untracked file — deleting it on a `worktree remove` — so
/// resolve that override over the raw-local config there (as [`crate::status::worktree_file_mode`] does
/// for `core.fileMode`). An *unreadable* config cannot establish a value, so default to git's
/// case-sensitive default (`false`) — the safe, non-folding direction; a *malformed* value is still an
/// error, as it is for git.
pub(crate) async fn ignore_case<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<bool, WorktreeError> {
	if wt.repository().has_effective_config() {
		return match wt.repository().effective_config().await {
			Ok(config) => Ok(
				config
					.get_bool_validated("core", None, "ignorecase")?
					.unwrap_or(false),
			),
			Err(_) => Ok(false),
		};
	}
	// Bare `Repository`: consult the per-worktree override (gated by the extension) over the raw-local
	// config, since no merged view carries it.
	let common = match wt.repository().read_config().await {
		Ok(common) => common,
		Err(_) => return Ok(false),
	};
	// We do not process `include`/`includeIf`; either could set `core.ignoreCase` after the direct value,
	// so a direct value cannot be trusted when includes are present — fail closed to `false` (the safe,
	// non-folding direction; a wrong fold could hide an untracked file from a `worktree remove`). Mirrors
	// [`crate::status::worktree_file_mode`].
	if crate::status::config_has_includes(&common) {
		return Ok(false);
	}
	if common
		.get_bool_validated("extensions", None, "worktreeconfig")?
		.unwrap_or(false)
	{
		match wt
			.repository()
			.objects()
			.file_store()
			.read_path("config.worktree")
			.await
		{
			Ok(bytes) => {
				// Present but non-UTF-8 → a value that cannot be established; fail closed to no-fold rather
				// than silently using the common value (which the override might have flipped to `false`).
				let Ok(text) = String::from_utf8(bytes) else {
					return Ok(false);
				};
				// A malformed `config.worktree` cannot establish a value either — fail closed to no-fold
				// (as for the non-UTF-8 case above), rather than propagating a parse error out of `status`.
				let Ok(over) = gitana_config::GitConfig::parse(&text) else {
					return Ok(false);
				};
				if crate::status::config_has_includes(&over) {
					return Ok(false);
				}
				if let Some(value) = over.get_bool_validated("core", None, "ignorecase")? {
					return Ok(value);
				}
				// Present and parseable but with no `core.ignoreCase` key — fall through to the common value.
			}
			// Absent → the common value applies.
			Err(gitana_file_store::FileStoreError::NotFound) => {}
			// Present but unreadable → fail closed to no-fold, as for a non-UTF-8 file.
			Err(_) => return Ok(false),
		}
	}
	Ok(
		common
			.get_bool_validated("core", None, "ignorecase")?
			.unwrap_or(false),
	)
}

/// The content of `.git/info/exclude`, or `None`. A directory at that path is fatal (git errors);
/// absent or unreadable (permission-denied) contributes no patterns, as git warns and continues.
pub(crate) async fn read_info_exclude<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
) -> Result<Option<String>, WorktreeError> {
	let store = wt.repository().objects().file_store();
	if store.is_dir("info/exclude").await.unwrap_or(false) {
		return Err(WorktreeError::ExcludeFile(".git/info/exclude".to_owned()));
	}
	match store.read_path("info/exclude").await {
		Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
		// Absent, or unreadable (permission-denied) — git warns and continues with no patterns.
		Err(_) => Ok(None),
	}
}
