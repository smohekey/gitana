use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::DiscoveryError;
use super::RepositoryLayout;

/// Walk up from `start` to find a repository: the nearest ancestor with a `.git` entry (its work
/// tree), or a directory that is itself a git directory (a bare repo). Like [`try_discover`], but a
/// genuine absence is reported as [`DiscoveryError::NotFound`] rather than `Ok(None)`.
pub async fn discover(start: &Path) -> Result<RepositoryLayout, DiscoveryError> {
	let owned = start.to_path_buf();
	blocking(move || discover_sync(&owned)).await
}

/// Like [`discover`], but distinguishes a genuine absence — `Ok(None)`, having walked to the
/// filesystem root without finding a repository — from a discovery *error* (a malformed `.git` file,
/// an unreadable gitdir, corrupt `commondir`, ...), which is returned as [`DiscoveryError`]. This lets
/// a caller fall back to ambient behaviour only when there truly is no repository, while still
/// aborting on a corrupted one, as git does.
///
/// `.git` may be a directory (an ordinary repository) or a file pointing at a per-worktree git
/// directory (a linked worktree created by `git worktree add`); the latter names its shared common
/// directory via a `commondir` file.
pub async fn try_discover(start: &Path) -> Result<Option<RepositoryLayout>, DiscoveryError> {
	let owned = start.to_path_buf();
	blocking(move || try_discover_sync(&owned)).await
}

/// Inspect exactly `root`, without searching ancestors: describe it if it is itself a repository root
/// (an ordinary work tree, a linked worktree, or a bare git directory), else
/// [`DiscoveryError::NotWorktreeRoot`].
pub async fn inspect_root(root: &Path) -> Result<RepositoryLayout, DiscoveryError> {
	let owned = root.to_path_buf();
	blocking(move || inspect_root_sync(&owned)).await
}

/// Resolve a (possibly per-worktree) `git_dir` to its shared common directory: the target of its
/// `commondir` file, or `git_dir` itself (canonicalized) when there is none — an ordinary or main git
/// directory shares nothing. Exposed for callers that hold a git directory directly and need its
/// common directory without re-running discovery.
pub async fn common_dir_of(git_dir: &Path) -> Result<PathBuf, DiscoveryError> {
	let owned = git_dir.to_path_buf();
	blocking(move || common_dir_of_sync(&owned)).await
}

/// Offload the synchronous filesystem discovery to tokio's blocking pool, keeping the reactor free.
/// Discovery is a short burst of metadata reads and canonicalizations; running it inline would block
/// the current-thread runtime the CLI drives.
async fn blocking<T, F>(f: F) -> T
where
	F: FnOnce() -> T + Send + 'static,
	T: Send + 'static,
{
	tokio::task::spawn_blocking(f)
		.await
		.expect("repo-layout discovery task panicked")
}

fn discover_sync(start: &Path) -> Result<RepositoryLayout, DiscoveryError> {
	// Report the caller's original `start` (not the canonical form) in a not-found message, matching
	// git's "or any parent up to /".
	try_discover_sync(start)?.ok_or_else(|| DiscoveryError::NotFound {
		start: start.to_path_buf(),
	})
}

fn try_discover_sync(start: &Path) -> Result<Option<RepositoryLayout>, DiscoveryError> {
	// Canonicalize once at the base, then pop lexically: every returned path is a component-prefix of a
	// canonical path, so it stays canonical without re-hitting the filesystem per level.
	let mut dir = canonicalize(start).map_err(|source| DiscoveryError::InaccessibleStart {
		path: start.to_path_buf(),
		source,
	})?;
	loop {
		if let Some(layout) = classify(&dir)? {
			return Ok(Some(layout));
		}
		if !dir.pop() {
			return Ok(None);
		}
	}
}

fn inspect_root_sync(root: &Path) -> Result<RepositoryLayout, DiscoveryError> {
	let root = canonicalize(root).map_err(|source| DiscoveryError::InaccessibleStart {
		path: root.to_path_buf(),
		source,
	})?;
	classify(&root)?.ok_or(DiscoveryError::NotWorktreeRoot { path: root })
}

/// Describe `dir` if it is itself a repository root, else `Ok(None)`. `dir` is assumed canonical (the
/// caller canonicalized it), so `worktree_root`/bare `git_dir` are canonical by construction; the
/// resolved git and common directories are canonicalized explicitly, which also resolves a symlinked
/// `.git` and honours a relative `gitdir`/`commondir` pointer.
fn classify(dir: &Path) -> Result<Option<RepositoryLayout>, DiscoveryError> {
	let git = dir.join(".git");
	// `is_dir`/`is_file` follow symlinks, so a `.git` symlink to a directory reads as a directory here,
	// as git treats it.
	if git.is_dir() {
		// A checkout whose `.git` is a directory. Canonicalize it (resolving a `.git` symlink to its
		// target), so `git_dir`/`common_dir` are canonical and stable for identity — a repo discovered
		// from its main checkout and from a linked worktree yields the same `common_dir`. The common dir
		// still comes from `commondir`: an ordinary main `.git` has none and is its own common dir, but a
		// `.git` symlink to a linked worktree's admin directory (which does carry `commondir`) resolves
		// to the shared main `.git`, as git accepts. The checkout is preserved separately in
		// `worktree_root` (the canonical directory we walked from).
		let git_dir = canonicalize(&git).map_err(|source| git_dir_error(git.clone(), source))?;
		let common_dir = common_dir_of_sync(&git_dir)?;
		return Ok(Some(RepositoryLayout {
			worktree_root: Some(dir.to_path_buf()),
			git_dir,
			common_dir,
		}));
	}
	if git.is_file() {
		let git_dir = resolve_gitdir_file(&git)?;
		let common_dir = common_dir_of_sync(&git_dir)?;
		return Ok(Some(RepositoryLayout {
			worktree_root: Some(dir.to_path_buf()),
			git_dir,
			common_dir,
		}));
	}
	if is_git_dir(dir) {
		// A bare/main git directory: `dir` is already canonical (walked from a canonical base) and has
		// no `commondir`, so it is its own common dir.
		return Ok(Some(RepositoryLayout {
			worktree_root: None,
			git_dir: dir.to_path_buf(),
			common_dir: dir.to_path_buf(),
		}));
	}
	// `.git` is neither a usable directory nor a valid gitdir file. Probe the entry itself with
	// `symlink_metadata` (which does not follow a symlink) to tell absence from corruption: only a
	// genuinely missing entry (`NotFound`) continues the walk to an ancestor. A *present* entry — most
	// notably a *dangling* `.git` symlink, where `is_dir`/`is_file` both follow the broken link and
	// report false — is corrupt metadata; and any *other* lookup failure (e.g. `PermissionDenied` when
	// the directory is not searchable) is inaccessible metadata. Both must error rather than be
	// misreported as "no repository here", which would let a caller take its outside-repository fallback.
	match std::fs::symlink_metadata(&git) {
		Ok(_) => {
			let source = canonicalize(&git)
				.err()
				.unwrap_or_else(|| std::io::Error::from(ErrorKind::NotFound));
			Err(git_dir_error(git, source))
		}
		Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
		Err(source) => Err(DiscoveryError::MissingGitDir { path: git, source }),
	}
}

/// Resolve a linked worktree's `.git` file to its canonical per-worktree git directory. The file holds
/// a single `gitdir: <path>` line; git writes an absolute path, but a relative one is resolved against
/// the worktree directory (the `.git` file's parent).
fn resolve_gitdir_file(git_file: &Path) -> Result<PathBuf, DiscoveryError> {
	// Read **bytes**, not a UTF-8 string: a linked worktree whose admin path is non-UTF-8 records a
	// non-UTF-8 `gitdir:` pointer (native paths are accepted without UTF-8 conversion), so a lossy
	// string read would wrongly reject it as malformed.
	let content = std::fs::read(git_file).map_err(|source| DiscoveryError::UnreadableGitFile {
		path: git_file.to_path_buf(),
		source,
	})?;
	// The first line (dropping a trailing `\r`, so a CRLF pointer file parses), then the `gitdir:` prefix,
	// then surrounding ASCII whitespace; an empty remainder is malformed.
	let first_line = content.split(|&b| b == b'\n').next().unwrap_or_default();
	let first_line = first_line.strip_suffix(b"\r").unwrap_or(first_line);
	let pointer = first_line
		.strip_prefix(b"gitdir:".as_slice())
		.map(|path| path.trim_ascii())
		.filter(|path| !path.is_empty())
		.ok_or_else(|| DiscoveryError::MalformedGitFile {
			path: git_file.to_path_buf(),
		})?;
	// A (non-Unix) non-representable pointer is malformed, not a lossily-mapped different path.
	let pointer = path_from_bytes(pointer).ok_or_else(|| DiscoveryError::MalformedGitFile {
		path: git_file.to_path_buf(),
	})?;
	let raw = if pointer.is_absolute() {
		pointer
	} else {
		git_file.parent().unwrap_or(Path::new(".")).join(pointer)
	};
	canonicalize(&raw).map_err(|source| git_dir_error(raw, source))
}

fn common_dir_of_sync(git_dir: &Path) -> Result<PathBuf, DiscoveryError> {
	let commondir = git_dir.join("commondir");
	let bytes = match std::fs::read(&commondir) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == ErrorKind::NotFound => {
			// A `read` NotFound is ambiguous: the file may be genuinely absent (a self-contained git
			// directory shares nothing — its common dir is itself), or `commondir` may be a *dangling*
			// symlink, whose entry exists but whose target does not. `symlink_metadata` does not follow
			// the link, so it tells them apart: a missing entry is absence; anything else is corrupt
			// metadata, which must error rather than silently mis-route to the per-worktree git dir.
			return match std::fs::symlink_metadata(&commondir) {
				Err(meta_error) if meta_error.kind() == ErrorKind::NotFound => {
					canonicalize(git_dir).map_err(|source| common_dir_error(git_dir.to_path_buf(), source))
				}
				_ => Err(DiscoveryError::MissingCommonDir {
					path: commondir,
					source: error,
				}),
			};
		}
		Err(source) => {
			return Err(DiscoveryError::UnreadableCommonDir {
				path: commondir,
				source,
			});
		}
	};
	// Parsed byte-clean (native paths accepted without UTF-8 conversion), so a non-UTF-8 common-dir
	// pointer resolves rather than being rejected as malformed.
	let pointer = bytes.trim_ascii();
	if pointer.is_empty() {
		return Err(DiscoveryError::MalformedCommonDir { path: commondir });
	}
	// A (non-Unix) non-representable pointer is malformed, not a lossily-mapped different path.
	let pointer = path_from_bytes(pointer).ok_or_else(|| DiscoveryError::MalformedCommonDir {
		path: commondir.clone(),
	})?;
	// `commondir` is typically `../..`, resolved against the git directory; canonicalize so every
	// linked worktree of a repository yields the same common dir.
	let common = git_dir.join(pointer);
	canonicalize(&common).map_err(|source| common_dir_error(common, source))
}

/// A path parsed from raw pointer-file bytes. **Byte-clean on Unix** (`OsStrExt::from_bytes`), so a
/// non-UTF-8 identity path round-trips exactly. On non-Unix, where byte-clean (WTF-8) parsing is a deferred
/// follow-up, this **fails closed** — `None` for invalid UTF-8 — rather than lossily map the bytes to a
/// *different* path that could resolve to the wrong repository; the caller then treats `None` as malformed.
fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;
		Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
	}
	#[cfg(not(unix))]
	{
		std::str::from_utf8(bytes).ok().map(PathBuf::from)
	}
}

/// Whether `dir` is itself a git directory (as a bare repo is): it holds `HEAD`, `objects/`, and
/// `refs/`.
fn is_git_dir(dir: &Path) -> bool {
	dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

fn canonicalize(path: &Path) -> Result<PathBuf, std::io::Error> {
	std::fs::canonicalize(path)
}

/// Map a git-directory canonicalization failure: a missing target is `MissingGitDir`; any other
/// failure (permission, a non-directory component, ...) is a canonicalization error.
fn git_dir_error(path: PathBuf, source: std::io::Error) -> DiscoveryError {
	if source.kind() == ErrorKind::NotFound {
		DiscoveryError::MissingGitDir { path, source }
	} else {
		DiscoveryError::Canonicalize { path, source }
	}
}

/// The common-directory counterpart to [`git_dir_error`].
fn common_dir_error(path: PathBuf, source: std::io::Error) -> DiscoveryError {
	if source.kind() == ErrorKind::NotFound {
		DiscoveryError::MissingCommonDir { path, source }
	} else {
		DiscoveryError::Canonicalize { path, source }
	}
}
