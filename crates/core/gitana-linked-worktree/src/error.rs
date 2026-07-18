//! Hard-failure errors. Refusals and conflicts are *not* errors — they are observations returned as
//! [`WorktreeClassification`](crate::WorktreeClassification) data inside `Ok`. A [`LinkedWorktreeError`]
//! is raised only when an operation cannot be carried out at all (I/O, corruption, a status computation
//! that fails). In particular a *status* failure is a [`LinkedWorktreeError`], never a "clean" reading.

use std::path::PathBuf;

/// Which on-disk pointer file was malformed (for [`LinkedWorktreeError::MalformedPointer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerKind {
	/// A checkout's `.git` gitfile (`gitdir: <admin>`).
	GitFile,
	/// A linked worktree admin directory's `gitdir` back-pointer.
	AdminGitdir,
	/// A `HEAD` file that is not valid UTF-8 / not a parseable ref-or-oid.
	Head,
	/// A `refs/…` symbolic-ref file whose chain does not resolve (an invalid target, or a cycle). Distinct
	/// from [`Head`](PointerKind::Head): the corruption is in a repository ref file reached *from* a valid
	/// starting point, not in the `HEAD` that rooted the walk.
	Ref,
}

impl PointerKind {
	fn as_str(self) -> &'static str {
		match self {
			PointerKind::GitFile => ".git gitfile",
			PointerKind::AdminGitdir => "admin gitdir",
			PointerKind::Head => "HEAD",
			PointerKind::Ref => "ref",
		}
	}
}

impl std::fmt::Display for PointerKind {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.as_str())
	}
}

/// A hard failure of a linked-worktree operation. Underlying error chains are preserved via
/// `#[from]`/`#[source]` so a consumer can inspect the cause; the `Display` is a one-line summary.
#[derive(Debug, thiserror::Error)]
pub enum LinkedWorktreeError {
	/// Repository discovery failed (walked past a filesystem boundary, a corrupt `.git` file, ...).
	#[error(transparent)]
	Discovery(#[from] gitana_repository_layout::DiscoveryError),

	/// A filesystem operation on a specific path failed.
	#[error("{context}: {}", .path.display())]
	Io {
		/// What was being attempted (e.g. "opening common dir", "reading HEAD").
		context: &'static str,
		/// The path the operation targeted.
		path: PathBuf,
		/// The underlying I/O error.
		#[source]
		source: std::io::Error,
	},

	/// An on-disk pointer file (`.git`, `gitdir`, `HEAD`) could not be parsed.
	#[error("malformed {kind} pointer at {}", .path.display())]
	MalformedPointer {
		/// Which pointer was malformed.
		kind: PointerKind,
		/// The pointer file's path.
		path: PathBuf,
	},

	/// A **caller-supplied** requested branch **name** is not a valid ref name. This is a bad *argument* —
	/// distinct from a [`MalformedPointer`], which covers on-disk corruption. The distinction is precise:
	/// only the *initial* name being invalid is this error; a *valid* branch whose on-disk symbolic-ref
	/// chain is broken or cyclic is repository corruption, reported as `MalformedPointer` of kind
	/// [`Ref`](PointerKind::Ref) — never blamed on the caller, and never on the repository's healthy `HEAD`.
	/// The name is echoed because the caller supplied it; no on-disk content is disclosed.
	#[error("invalid requested branch: {0}")]
	InvalidRequestedBranch(String),

	/// A repository-engine operation failed (ref resolution, HEAD parse, config read, ...).
	#[error(transparent)]
	Repository(#[from] gitana_repository::RepositoryError),

	/// A working-tree operation failed — notably a `status` computation, which must never be silently
	/// treated as a clean worktree.
	#[error(transparent)]
	Worktree(#[from] gitana_worktree::WorktreeError),

	/// The repository declares an object format gitana does not support.
	#[error("unsupported object format: {0}")]
	UnsupportedObjectFormat(String),

	/// A hex object id could not be parsed for the requested algorithm (wrong length / non-hex).
	#[error("invalid {kind} object id: {hex}", kind = .kind.name())]
	InvalidObjectId {
		/// The algorithm the id was parsed for.
		kind: gitana_object::HashKind,
		/// The offending hex string.
		hex: String,
	},

	/// An identity path (a destination, common dir, or discovery start) was relative. Such paths must be
	/// absolute — resolving a relative path would consult the process current directory, which this crate
	/// never does.
	#[error("path must be absolute: {}", .0.display())]
	RelativePath(PathBuf),

	/// `core.bare` held a value that is not a valid git boolean.
	#[error("invalid boolean for core.bare in {}", .0.display())]
	InvalidCoreBare(PathBuf),

	/// The per-repository worktree-registration lock (`<common>/worktrees.lock`) is held by another
	/// worktree create/remove and did not free within the brief retry window — a **lost race, reported as
	/// a retryable conflict** rather than overwriting the concurrent operation. A leftover lock from a
	/// crashed process surfaces the same way (gitana does not auto-break it, matching its ref locks); it is
	/// cleared by removing the file. Carries the lock path.
	#[error("worktree registration is locked by another operation: {}", .0.display())]
	RegistrationLocked(PathBuf),
}

impl LinkedWorktreeError {
	/// Build an [`Io`](LinkedWorktreeError::Io) error tagging the attempted operation and path. Only the
	/// native reading layer constructs these, so it is native-only (keeps the wasm build warning-free).
	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn io(
		context: &'static str,
		path: impl Into<PathBuf>,
		source: std::io::Error,
	) -> Self {
		LinkedWorktreeError::Io {
			context,
			path: path.into(),
			source,
		}
	}
}
