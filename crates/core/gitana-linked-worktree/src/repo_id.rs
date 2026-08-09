//! Explicit repository identity, and the native capability mint.
//!
//! The identity anchor is the shared **common dir** (`objects`/`refs`/`config`): linked worktrees of
//! one repository share it, and a destination path never identifies a repository — so ownership is
//! never inferred from a destination. `discover` resolves an identity from a start path (ordinary,
//! bare, or from *inside* a linked worktree); `at_common_dir` names one directly with no walk and no
//! process-CWD read.

use std::path::{Path, PathBuf};

use gitana_repository_layout::RepositoryLayout;

use crate::LinkedWorktreeError;

/// An explicitly-identified repository. All paths are absolute; nothing here reads the process CWD.
///
/// The fields are private and every constructor validates that the paths are absolute, so a caller
/// cannot build (or mutate) an identity with a relative path that the I/O entry points would later
/// resolve against the process current directory.
#[derive(Debug, Clone)]
pub struct RepositoryId {
	common_dir: PathBuf,
	git_dir: PathBuf,
	worktree_root: Option<PathBuf>,
}

/// Identity is the **shared common dir** (the documented anchor), so two `RepositoryId`s naming the same
/// repository — e.g. the same repo discovered from its primary vs a linked worktree, which yield identical
/// `common_dir` but different contextual `git_dir`/`worktree_root` — compare equal. Deriving equality over
/// all fields would report them as different repositories and break consumer identity checks.
impl PartialEq for RepositoryId {
	fn eq(&self, other: &Self) -> bool {
		self.common_dir == other.common_dir
	}
}
impl Eq for RepositoryId {}

impl RepositoryId {
	/// The shared `.git` (`objects`/`refs`/`config`) — the identity anchor.
	pub fn common_dir(&self) -> &Path {
		&self.common_dir
	}

	/// The per-worktree git directory of the discovery context (`== common_dir` for an ordinary repo,
	/// or a `<common>/worktrees/<name>` when discovered from inside a linked worktree).
	pub fn git_dir(&self) -> &Path {
		&self.git_dir
	}

	/// The discovery context's working-tree root; `None` for a bare context or an `at_common_dir` identity.
	pub fn worktree_root(&self) -> Option<&Path> {
		self.worktree_root.as_deref()
	}

	/// Discover the repository containing `start` (an ordinary checkout, a bare repo, or a linked
	/// worktree). The `common_dir` resolves to the shared `.git` regardless of which worktree `start` is
	/// in, so the identity is stable across worktrees. `start` must be **absolute** — a relative start
	/// would be resolved against the process current directory.
	pub async fn discover(start: &Path) -> Result<Self, LinkedWorktreeError> {
		if !start.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(start.to_path_buf()));
		}
		Self::from_layout(gitana_repository_layout::discover(start).await?)
	}

	/// Build (and validate) an identity from a discovered [`RepositoryLayout`]. Private and validated so a
	/// caller cannot bypass the absolute-path invariant by hand-crafting a layout and converting it — a
	/// discovered layout is already canonical/absolute, so this only guards against misuse.
	fn from_layout(layout: RepositoryLayout) -> Result<Self, LinkedWorktreeError> {
		if !layout.common_dir.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(layout.common_dir));
		}
		if !layout.git_dir.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(layout.git_dir));
		}
		Ok(RepositoryId {
			common_dir: layout.common_dir,
			git_dir: layout.git_dir,
			worktree_root: layout.worktree_root,
		})
	}

	/// Name a repository directly by its shared common dir — fully explicit, no discovery walk. Use this
	/// when the caller already knows the repository's `.git`. `git_dir` is taken to equal `common_dir`
	/// (an ordinary/main context) and `worktree_root` is left unknown. `common_dir` must be **absolute**
	/// (a relative identity would later open against the process current directory).
	pub fn at_common_dir(common_dir: PathBuf) -> Result<Self, LinkedWorktreeError> {
		if !common_dir.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(common_dir));
		}
		// Resolve symlink / `.` / `..` aliases to the real directory, as git does before inferring layout —
		// otherwise a `meta-link -> repo/.git` alias would be judged by the basename `meta-link` (marked bare,
		// primary path reported as the link). The `is_absolute` check above runs first, so a *relative* path
		// is rejected rather than canonicalized against the process CWD. A not-yet-existing path is kept as
		// given (nothing to resolve).
		let common_dir = common_dir.canonicalize().unwrap_or(common_dir);
		Ok(RepositoryId {
			git_dir: common_dir.clone(),
			common_dir,
			worktree_root: None,
		})
	}
}

/// The native capability mint. `cap-std` does not build on wasm, so this — and every function that
/// opens a store — is native-only; the pure types and classification stay available everywhere.
#[cfg(not(target_arch = "wasm32"))]
mod native {
	use super::*;

	use cap_std::ambient_authority;
	use cap_std::fs::Dir;
	use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};
	use gitana_object::HashKind;
	use gitana_repository::{RepositoryError, detect_hash_kind};

	/// Detect the repository's object format, mapping an unsupported format to the documented
	/// [`LinkedWorktreeError::UnsupportedObjectFormat`] variant (the raw `detect_hash_kind` returns a
	/// `RepositoryError` the `?` would otherwise wrap opaquely). Used by every entry point so the
	/// format-specific failure is matchable.
	pub(crate) async fn detect_kind(
		store: &WorktreeFileStore,
	) -> Result<HashKind, LinkedWorktreeError> {
		match detect_hash_kind(store).await {
			Ok(kind) => Ok(kind),
			Err(RepositoryError::UnsupportedFormat(msg)) => {
				Err(LinkedWorktreeError::UnsupportedObjectFormat(msg))
			}
			Err(other) => Err(other.into()),
		}
	}

	/// Open the two directories of a repository as a routing file store, minting `cap-std` authority
	/// from **absolute** paths (never changing the process CWD). This deliberately does *not* install
	/// git's global/system effective config (a user-environment concern the CLI edge handles); slice-1
	/// reads only the repository-local `config`.
	pub(crate) fn open_store_raw(
		git_dir: &Path,
		common_dir: &Path,
	) -> Result<WorktreeFileStore, LinkedWorktreeError> {
		let common = Dir::open_ambient_dir(common_dir, ambient_authority())
			.map_err(|e| LinkedWorktreeError::io("opening common dir", common_dir, e))?;
		let git = Dir::open_ambient_dir(git_dir, ambient_authority())
			.map_err(|e| LinkedWorktreeError::io("opening git dir", git_dir, e))?;
		Ok(WorktreeFileStore::new(common, git))
	}

	/// Open a working-tree directory as a filesystem capability (for a status readout).
	pub(crate) fn open_work_dir(work: &Path) -> Result<CapWorkDir, LinkedWorktreeError> {
		let dir = Dir::open_ambient_dir(work, ambient_authority())
			.map_err(|e| LinkedWorktreeError::io("opening work tree", work, e))?;
		Ok(CapWorkDir::from_dir(dir))
	}

	/// Require the files-layout anchors git uses to recognize the shared repository directory.
	///
	/// Metadata follows symlinks, matching git's acceptance of legacy symbolic `HEAD` files and
	/// symlinked repository directories. Missing, unreadable, or wrong-kind entries fail closed at the
	/// exact path instead of letting higher-level readers mistake an arbitrary directory for a repository.
	pub(crate) fn validate_repository_structure(common: &Path) -> Result<(), LinkedWorktreeError> {
		require_repository_entry(&common.join("HEAD"), "a file", std::fs::Metadata::is_file)?;
		require_repository_entry(
			&common.join("objects"),
			"a directory",
			std::fs::Metadata::is_dir,
		)?;
		require_repository_entry(
			&common.join("refs"),
			"a directory",
			std::fs::Metadata::is_dir,
		)
	}

	fn require_repository_entry(
		path: &Path,
		expected: &'static str,
		matches: fn(&std::fs::Metadata) -> bool,
	) -> Result<(), LinkedWorktreeError> {
		let metadata = std::fs::metadata(path)
			.map_err(|error| LinkedWorktreeError::io("inspecting repository structure", path, error))?;
		if !matches(&metadata) {
			return Err(LinkedWorktreeError::io(
				"validating repository structure",
				path,
				std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					format!("expected {expected}"),
				),
			));
		}
		Ok(())
	}

	/// Reject a repository whose common `config` declares a **format gitana cannot safely read or mutate** —
	/// git's own `check_repository_format` refuses to operate on such a repo, so every structural reader or
	/// writer must fail closed the same way (requirements 257-258). Reproduces git's format grammar exactly
	/// (each rule verified against stock `git worktree move`), reading only the config (no object store):
	///
	/// - **`core.repositoryformatversion`** — validated at *every* occurrence (git parses it eagerly and
	///   aborts on any malformed value, even one a later valid value shadows, and even at version 0); then the
	///   effective version must be `<= 1` (git: "Expected git repo version <= 1").
	/// - **`extensions.*`** — read only at version `>= 1` (git ignores them at version 0). The allowlist is
	///   exactly the git-recognized extensions that do not change how gitana must read this repo for a
	///   *structural* pointer op:
	///   - `objectformat` — the object hash (the effective value is also validated by [`detect_kind`]).
	///   - `worktreeconfig`, `relativeworktrees` — worktree admin layout gitana already handles.
	///   - `partialclone`, `preciousobjects` — object-store *policy* (promisor objects; never-prune); a move
	///     or removal touches no objects, so these are safe. git moves such repos; refusing them blocked a
	///     supported move (e.g. any partial clone).
	///   - `noop` — git's test extension.
	///
	///   Any other extension name is refused. The whole remainder after `extensions.` is git's extension name
	///   — a config *subsection* is part of it (`[extensions "foo"] bar` → `extensions.foo.bar`, which git
	///   rejects as unknown `foo.bar`), so a dotted remainder is matched, not skipped.
	///
	///   The *value* of each known extension is validated at every occurrence, exactly as git does: the bool
	///   extensions (`worktreeconfig`, `relativeworktrees`, `preciousobjects`) must be git booleans;
	///   `partialclone`/`objectformat` must carry a value (a valueless occurrence is "missing value"); and
	///   each `objectformat` value must name a git-recognized format (case-sensitive `sha1`/`sha256`). `noop`
	///   takes any value.
	///
	/// Deliberately **excluded** (so still refused, a fail-closed divergence from git, which does move it):
	/// `refstorage` — reftable changes the **ref backend**, which gitana's structural HEAD/ref reads assume is
	/// the files format; that is precisely a format gitana does not fully understand, so it fails closed.
	///
	/// A missing/unparseable config is left to `detect_kind`'s `UnsupportedObjectFormat`. Shared by
	/// enumeration and the destructive entry points (`remove`, `relocate`).
	pub(crate) fn reject_unsupported_repository_format(
		common: &Path,
	) -> Result<(), LinkedWorktreeError> {
		const KNOWN: &[&str] = &[
			"objectformat",
			"worktreeconfig",
			"relativeworktrees",
			"partialclone",
			"preciousobjects",
			"noop",
		];
		// git parses each of these as a boolean at every occurrence.
		const BOOL: &[&str] = &["worktreeconfig", "relativeworktrees", "preciousobjects"];
		// git requires a value for each of these (a valueless occurrence is an error).
		const VALUE_REQUIRED: &[&str] = &["partialclone", "objectformat"];

		let Ok(bytes) = std::fs::read(common.join("config")) else {
			return Ok(());
		};
		let Ok(config) = gitana_config::GitConfigBytes::parse(&bytes) else {
			return Ok(());
		};
		let bad = LinkedWorktreeError::UnsupportedObjectFormat;

		// Validate every `repositoryformatversion` occurrence (eager, even at v0), then bound the effective one.
		let version = config
			.get_int_validated("core", None, "repositoryformatversion")
			.map_err(|e| bad(format!("invalid core.repositoryformatversion: {e}")))?
			.unwrap_or(0);
		if version > 1 {
			return Err(bad(format!(
				"unsupported repository format version {version}"
			)));
		}
		if version < 1 {
			return Ok(());
		}

		// Reject an unknown extension name (the whole dotted remainder).
		for (key, _) in config.entries() {
			if let Some(name) = key.strip_prefix(b"extensions.")
				&& !KNOWN
					.iter()
					.any(|known| name.eq_ignore_ascii_case(known.as_bytes()))
			{
				return Err(bad(format!(
					"unknown repository extension: extensions.{}",
					String::from_utf8_lossy(name)
				)));
			}
		}

		// Validate the *value* of each known extension at every occurrence, as git does.
		for name in BOOL {
			config
				.get_bool_validated("extensions", None, name)
				.map_err(|e| bad(format!("invalid value for extensions.{name}: {e}")))?;
		}
		for name in VALUE_REQUIRED {
			if config
				.get_all_raw("extensions", None, name)
				.iter()
				.any(Option::is_none)
			{
				return Err(bad(format!("missing value for extensions.{name}")));
			}
		}
		for raw in config.get_all_raw("extensions", None, "objectformat") {
			if let Some(value) = raw
				&& value.as_slice() != b"sha1"
				&& value.as_slice() != b"sha256"
			{
				return Err(bad(format!(
					"invalid value for extensions.objectformat: {}",
					String::from_utf8_lossy(&value)
				)));
			}
		}
		Ok(())
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{
	detect_kind, open_store_raw, open_work_dir, reject_unsupported_repository_format,
	validate_repository_structure,
};
