use std::path::PathBuf;

/// A discovered repository's on-disk layout: where its working tree, its (possibly per-worktree) git
/// directory, and its shared *common* directory live.
///
/// All paths are canonical and absolute. For an ordinary repository `git_dir` and `common_dir` are
/// the same path; they diverge only for a linked worktree (`git worktree add`), whose per-worktree
/// git directory lives under the main repository's shared common directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLayout {
	/// The working-tree root, or `None` for a bare repository. Usually the folder holding `.git`
	/// ([`marker_root`](Self::marker_root)), but the repository's `core.worktree` can put it elsewhere,
	/// as git does.
	pub worktree_root: Option<PathBuf>,
	/// The folder holding the `.git` discovery found (a directory, or a file pointing at one), or `None`
	/// for a git directory found as itself (a bare repository, or the inside of a `.git`). The same as
	/// `worktree_root` unless `core.worktree` moves the work tree: re-check a layout by inspecting this
	/// folder ([`inspect_root`](crate::inspect_root)), not the work tree, which needn't hold a `.git`.
	pub marker_root: Option<PathBuf>,
	/// The git directory holding this checkout's per-worktree files (`HEAD`, `index`, ...). For an
	/// ordinary repository this is `<worktree_root>/.git`; for a linked worktree it is
	/// `<main>/.git/worktrees/<name>`.
	pub git_dir: PathBuf,
	/// The shared directory holding `objects`, `refs`, `config`, ... For an ordinary repository this
	/// equals `git_dir`; for a linked worktree it is the main `.git`. Different linked worktrees of the
	/// same repository resolve to the same `common_dir`.
	pub common_dir: PathBuf,
	/// Whether the repository's config sets both `core.bare` true and `core.worktree`, which git takes
	/// as bare (no work tree) with the warning "core.bare and core.worktree do not make sense", for a
	/// command to give as git does. `false` for any other layout.
	pub bare_and_worktree: bool,
}
