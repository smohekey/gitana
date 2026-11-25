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
	use std::path::Path;

	use cap_std::ambient_authority;
	use cap_std::fs::Dir;
	use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};
	use gitana_object::HashKind;
	use gitana_repository::{Config, RepositoryError, detect_hash_kind};

	use crate::LinkedWorktreeError;

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

	/// Detect the object format from the common config's ambient repository path. Enumeration uses
	/// this after its caller has serialized and validated the config namespace so a supported config
	/// symlink is followed instead of rejected by the confined repository file store.
	pub(crate) fn detect_kind_at(common_dir: &Path) -> Result<HashKind, LinkedWorktreeError> {
		let config_path = common_dir.join("config");
		let bytes = match std::fs::read(&config_path) {
			Ok(bytes) => bytes,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				return Err(LinkedWorktreeError::UnsupportedObjectFormat(
					"no config file".to_owned(),
				));
			}
			Err(error) => {
				return Err(LinkedWorktreeError::io(
					"reading repository format",
					&config_path,
					error,
				));
			}
		};
		match Config::parse_bytes(&bytes) {
			Ok(config) if config.object_format == "sha256" => Ok(HashKind::Sha256),
			Ok(_) => Ok(HashKind::Sha1),
			Err(RepositoryError::UnsupportedFormat(message)) => {
				Err(LinkedWorktreeError::UnsupportedObjectFormat(message))
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

	/// Reject a repository whose common `config` declares a **format gitana cannot safely read or mutate**,
	/// through the shared repository-format check every entry point uses (`Config::parse_bytes`: git's own
	/// rules for the format version and its extensions, plus what gitana refuses on top, such as reftable).
	/// git refuses to operate on a repository whose format it doesn't understand, so the structural readers
	/// and writers fail closed the same way (requirements 257-258).
	///
	/// It reads the config through its ambient path, before any object store is opened. A missing or
	/// unparseable config is left to `detect_kind`'s `UnsupportedObjectFormat`. Shared by enumeration and
	/// the destructive entry points (`remove`, `relocate`); `create` and `inspect` reach the same check
	/// through `detect_kind`.
	pub(crate) fn reject_unsupported_repository_format(
		common: &Path,
	) -> Result<(), LinkedWorktreeError> {
		let Ok(bytes) = std::fs::read(common.join("config")) else {
			return Ok(());
		};
		if gitana_config::GitConfigBytes::parse(&bytes).is_err() {
			return Ok(());
		}
		match Config::parse_bytes(&bytes) {
			Ok(_) => Ok(()),
			Err(RepositoryError::UnsupportedFormat(message)) => {
				Err(LinkedWorktreeError::UnsupportedObjectFormat(message))
			}
			Err(other) => Err(other.into()),
		}
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use self::native::{
	detect_kind, detect_kind_at, open_store_raw, open_work_dir, reject_unsupported_repository_format,
	validate_repository_structure,
};
