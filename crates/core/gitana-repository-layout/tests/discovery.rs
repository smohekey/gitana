//! Discovery behaviour over hand-built fixtures, plus one linked-worktree fixture produced by stock
//! git as an oracle.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_repository_layout::{
	DiscoveryError, common_dir_of, discover, inspect_root, inspect_source, try_discover,
};
use tempfile::TempDir;

/// Canonicalize a path for comparison against discovery's canonical output.
fn canon(path: &Path) -> PathBuf {
	fs::canonicalize(path).expect("canonicalize fixture path")
}

/// Build the minimal contents of a bare/main git directory: `HEAD`, `objects/`, `refs/`.
fn write_git_dir_markers(git_dir: &Path) {
	fs::create_dir_all(git_dir.join("objects")).unwrap();
	fs::create_dir_all(git_dir.join("refs")).unwrap();
	fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
}

/// An ordinary repository: `<root>/.git` is a directory.
fn ordinary_repo(root: &Path) {
	write_git_dir_markers(&root.join(".git"));
}

#[tokio::test]
async fn ordinary_repository_reports_matching_git_and_common_dirs() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path();
	ordinary_repo(root);

	let layout = discover(root).await.unwrap();
	assert_eq!(layout.worktree_root.as_deref(), Some(canon(root).as_path()));
	assert_eq!(layout.git_dir, canon(&root.join(".git")));
	assert_eq!(layout.common_dir, layout.git_dir);
}

#[cfg(unix)]
#[tokio::test]
async fn ordinary_git_symlink_yields_canonical_stable_paths() {
	// `.git` is a symlink to an external git directory. git_dir/common_dir must be the *canonical*
	// external target (not the lexical symlink), so identity is stable — and the checkout is preserved
	// separately in worktree_root. Re-resolving common_dir_of(git_dir) must agree with common_dir.
	let tmp = TempDir::new().unwrap();
	let work = tmp.path().join("checkout");
	fs::create_dir_all(&work).unwrap();
	let external = tmp.path().join("external-gitdir");
	write_git_dir_markers(&external);
	std::os::unix::fs::symlink(&external, work.join(".git")).unwrap();

	let layout = discover(&work).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&work).as_path())
	);
	assert_eq!(layout.git_dir, canon(&external));
	assert_eq!(layout.common_dir, canon(&external));
	assert_eq!(
		common_dir_of(&layout.git_dir).await.unwrap(),
		layout.common_dir
	);
}

#[cfg(unix)]
#[tokio::test]
async fn git_symlink_to_admin_dir_resolves_shared_common_dir() {
	// `.git` is a *directory* symlink pointing at a linked worktree's admin directory (which carries a
	// `commondir`). git resolves the shared dir through that pointer; discovery must too, not treat the
	// admin dir as its own common dir.
	let tmp = TempDir::new().unwrap();
	let base = tmp.path();
	let main_git = base.join("repo/.git");
	write_git_dir_markers(&main_git);
	let admin = main_git.join("worktrees/feature");
	fs::create_dir_all(&admin).unwrap();
	write_git_dir_markers(&admin);
	fs::write(admin.join("commondir"), "../..\n").unwrap();

	let work = base.join("checkout");
	fs::create_dir_all(&work).unwrap();
	std::os::unix::fs::symlink(&admin, work.join(".git")).unwrap();

	let layout = discover(&work).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&work).as_path())
	);
	assert_eq!(layout.git_dir, canon(&admin));
	assert_eq!(layout.common_dir, canon(&main_git));
}

#[tokio::test]
async fn inspect_root_examines_exactly_the_given_path() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path();
	ordinary_repo(root);

	let layout = inspect_root(root).await.unwrap();
	assert_eq!(layout.worktree_root.as_deref(), Some(canon(root).as_path()));
}

#[tokio::test]
async fn discovery_walks_up_from_a_subdirectory() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path();
	ordinary_repo(root);
	let sub = root.join("a/b/c");
	fs::create_dir_all(&sub).unwrap();

	let layout = discover(&sub).await.unwrap();
	assert_eq!(layout.worktree_root.as_deref(), Some(canon(root).as_path()));
}

#[tokio::test]
async fn inspect_root_refuses_an_ancestor_repository() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path();
	ordinary_repo(root);
	let sub = root.join("sub");
	fs::create_dir_all(&sub).unwrap();

	// A subdirectory is not itself a repository root, even though an ancestor is.
	let error = inspect_root(&sub).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::NotWorktreeRoot { .. }),
		"{error:?}"
	);
}

#[tokio::test]
async fn bare_repository_has_no_worktree_root() {
	let tmp = TempDir::new().unwrap();
	let bare = tmp.path().join("example.git");
	write_git_dir_markers(&bare);

	let layout = discover(&bare).await.unwrap();
	assert_eq!(layout.worktree_root, None);
	assert_eq!(layout.git_dir, canon(&bare));
	assert_eq!(layout.common_dir, canon(&bare));
}

/// Build a linked worktree by hand: a `.git` *file* pointing at a per-worktree git dir that carries a
/// `commondir` back to the main `.git`.
fn hand_built_linked_worktree(
	absolute_gitdir: bool,
	relative_commondir: bool,
) -> (TempDir, PathBuf, PathBuf, PathBuf) {
	let tmp = TempDir::new().unwrap();
	let base = tmp.path();
	let main_git = base.join("repo/.git");
	write_git_dir_markers(&main_git);

	let admin = main_git.join("worktrees/feature");
	fs::create_dir_all(&admin).unwrap();
	write_git_dir_markers(&admin);

	let commondir_value = if relative_commondir {
		"../..".to_owned()
	} else {
		canon(&main_git).to_string_lossy().into_owned()
	};
	fs::write(admin.join("commondir"), format!("{commondir_value}\n")).unwrap();

	let worktree = base.join("worktrees/feature");
	fs::create_dir_all(&worktree).unwrap();
	let gitdir_value = if absolute_gitdir {
		canon(&admin).to_string_lossy().into_owned()
	} else {
		// Relative to the worktree directory (the `.git` file's parent).
		let rel = pathdiff(&worktree, &admin);
		rel.to_string_lossy().into_owned()
	};
	fs::write(worktree.join(".git"), format!("gitdir: {gitdir_value}\n")).unwrap();

	(tmp, worktree, admin, main_git)
}

/// A crude relative path from `from` to `to`, sufficient for fixtures under a shared temp root.
fn pathdiff(from: &Path, to: &Path) -> PathBuf {
	let from = canon(from);
	let to = canon(to);
	let common: PathBuf = from
		.components()
		.zip(to.components())
		.take_while(|(a, b)| a == b)
		.map(|(a, _)| a.as_os_str())
		.collect();
	let ups = from.strip_prefix(&common).unwrap().components().count();
	let mut rel = PathBuf::new();
	for _ in 0..ups {
		rel.push("..");
	}
	rel.push(to.strip_prefix(&common).unwrap());
	rel
}

#[tokio::test]
async fn linked_worktree_absolute_gitdir_resolves_common_dir() {
	let (_tmp, worktree, admin, main_git) = hand_built_linked_worktree(true, true);

	let layout = discover(&worktree).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&worktree).as_path())
	);
	assert_eq!(layout.git_dir, canon(&admin));
	assert_eq!(layout.common_dir, canon(&main_git));
}

#[tokio::test]
async fn linked_worktree_relative_gitdir_resolves_common_dir() {
	let (_tmp, worktree, admin, main_git) = hand_built_linked_worktree(false, true);

	let layout = discover(&worktree).await.unwrap();
	assert_eq!(layout.git_dir, canon(&admin));
	assert_eq!(layout.common_dir, canon(&main_git));
}

#[tokio::test]
async fn linked_worktree_absolute_commondir_resolves() {
	let (_tmp, worktree, _admin, main_git) = hand_built_linked_worktree(true, false);

	let layout = discover(&worktree).await.unwrap();
	assert_eq!(layout.common_dir, canon(&main_git));
}

#[cfg(unix)]
#[tokio::test]
async fn discovery_resolves_through_a_symlinked_start() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path();
	ordinary_repo(root);
	let sub = root.join("real");
	fs::create_dir_all(&sub).unwrap();
	let link = root.join("link");
	std::os::unix::fs::symlink(&sub, &link).unwrap();

	let layout = discover(&link).await.unwrap();
	// The worktree root is the canonical (physical) repository root, not the lexical link path.
	assert_eq!(layout.worktree_root.as_deref(), Some(canon(root).as_path()));
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_git_symlink_is_an_error_not_absence() {
	// `.git` is a symlink whose target is gone: `is_dir`/`is_file` both follow the broken link and
	// report false, but the entry exists, so this is corrupt metadata, not absence.
	let tmp = TempDir::new().unwrap();
	let worktree = tmp.path();
	std::os::unix::fs::symlink("/no/such/git/dir", worktree.join(".git")).unwrap();

	let error = discover(worktree).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MissingGitDir { .. }),
		"{error:?}"
	);
}

#[cfg(unix)]
#[tokio::test]
async fn inaccessible_metadata_is_an_error_not_absence() {
	use std::os::unix::fs::PermissionsExt;

	// A start directory that exists but is not searchable: probing `.git` fails with PermissionDenied,
	// which is inaccessible metadata, not absence — discovery must error, not walk to an ancestor.
	let tmp = TempDir::new().unwrap();
	let locked = tmp.path().join("locked");
	fs::create_dir(&locked).unwrap();
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

	// Skip when the mode is not enforced (e.g. running as root): the scenario cannot be set up.
	if fs::read_dir(&locked).is_ok() {
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
		return;
	}

	let result = try_discover(&locked).await;
	// Restore permissions so the TempDir can be cleaned up, regardless of the assertion below.
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

	let error = result.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MissingGitDir { .. }),
		"{error:?}"
	);
}

#[tokio::test]
async fn malformed_git_file_is_an_error() {
	let tmp = TempDir::new().unwrap();
	let worktree = tmp.path();
	fs::write(worktree.join(".git"), "this is not a gitdir pointer\n").unwrap();

	let error = discover(worktree).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MalformedGitFile { .. }),
		"{error:?}"
	);
}

#[tokio::test]
async fn empty_git_file_is_an_error() {
	let tmp = TempDir::new().unwrap();
	let worktree = tmp.path();
	fs::write(worktree.join(".git"), "gitdir:   \n").unwrap();

	let error = discover(worktree).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MalformedGitFile { .. }),
		"{error:?}"
	);
}

#[tokio::test]
async fn missing_gitdir_target_is_an_error() {
	let tmp = TempDir::new().unwrap();
	let worktree = tmp.path();
	fs::write(worktree.join(".git"), "gitdir: /no/such/git/dir\n").unwrap();

	let error = discover(worktree).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MissingGitDir { .. }),
		"{error:?}"
	);
}

#[tokio::test]
async fn empty_commondir_is_an_error() {
	let tmp = TempDir::new().unwrap();
	let base = tmp.path();
	let admin = base.join("repo/.git/worktrees/feature");
	fs::create_dir_all(&admin).unwrap();
	write_git_dir_markers(&admin);
	fs::write(admin.join("commondir"), "   \n").unwrap();
	let worktree = base.join("wt");
	fs::create_dir_all(&worktree).unwrap();
	fs::write(
		worktree.join(".git"),
		format!("gitdir: {}\n", canon(&admin).display()),
	)
	.unwrap();

	let error = discover(&worktree).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MalformedCommonDir { .. }),
		"{error:?}"
	);
}

#[tokio::test]
async fn missing_commondir_target_is_an_error() {
	let tmp = TempDir::new().unwrap();
	let base = tmp.path();
	let admin = base.join("repo/.git/worktrees/feature");
	fs::create_dir_all(&admin).unwrap();
	write_git_dir_markers(&admin);
	fs::write(admin.join("commondir"), "/no/such/common/dir\n").unwrap();
	let worktree = base.join("wt");
	fs::create_dir_all(&worktree).unwrap();
	fs::write(
		worktree.join(".git"),
		format!("gitdir: {}\n", canon(&admin).display()),
	)
	.unwrap();

	let error = discover(&worktree).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MissingCommonDir { .. }),
		"{error:?}"
	);
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_commondir_symlink_is_an_error_not_absence() {
	// A `commondir` that is a dangling symlink: the entry exists (so it is not a self-contained git
	// dir) but its target is gone. This is corrupt metadata, and must error rather than silently
	// treating the worktree as its own common dir.
	let tmp = TempDir::new().unwrap();
	let base = tmp.path();
	let admin = base.join("repo/.git/worktrees/feature");
	fs::create_dir_all(&admin).unwrap();
	write_git_dir_markers(&admin);
	std::os::unix::fs::symlink("/no/such/target", admin.join("commondir")).unwrap();
	let worktree = base.join("wt");
	fs::create_dir_all(&worktree).unwrap();
	fs::write(
		worktree.join(".git"),
		format!("gitdir: {}\n", canon(&admin).display()),
	)
	.unwrap();

	let error = discover(&worktree).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MissingCommonDir { .. }),
		"{error:?}"
	);
}

#[tokio::test]
async fn genuine_absence_is_ok_none() {
	// A plain directory tree with no repository anywhere up to the temp root's ancestors.
	let tmp = TempDir::new().unwrap();
	let sub = tmp.path().join("a/b");
	fs::create_dir_all(&sub).unwrap();

	// `try_discover` may find a repository in a real ancestor of the temp dir on some machines; guard
	// by asserting only that it does not *error*. The corrupt-metadata cases above prove the error path.
	let found = try_discover(&sub).await.unwrap();
	// If anything is found it must be an ancestor, never inside our clean subtree.
	if let Some(layout) = found {
		let root = layout.worktree_root.unwrap_or(layout.git_dir);
		assert!(
			!canon(&sub).starts_with(canon(&root).join("a")),
			"unexpectedly matched inside the clean subtree"
		);
	}
}

#[tokio::test]
async fn corrupt_metadata_errors_rather_than_reporting_absence() {
	// try_discover must surface a corrupt repository as an error, not `Ok(None)`.
	let tmp = TempDir::new().unwrap();
	let worktree = tmp.path().join("wt");
	fs::create_dir_all(&worktree).unwrap();
	fs::write(worktree.join(".git"), "gitdir: /no/such/git/dir\n").unwrap();

	let error = try_discover(&worktree).await.unwrap_err();
	assert!(
		matches!(error, DiscoveryError::MissingGitDir { .. }),
		"{error:?}"
	);
}

// --- stock-git oracle -------------------------------------------------------

/// Run `git` in `dir`, returning success. Skips (returns `false`) when git is unavailable.
fn git(dir: &Path, args: &[&str]) -> bool {
	Command::new("git")
		.current_dir(dir)
		.args(args)
		.env("GIT_AUTHOR_NAME", "T")
		.env("GIT_AUTHOR_EMAIL", "t@example.com")
		.env("GIT_COMMITTER_NAME", "T")
		.env("GIT_COMMITTER_EMAIL", "t@example.com")
		.output()
		.map(|out| out.status.success())
		.unwrap_or(false)
}

#[tokio::test]
async fn stock_git_linked_worktree_resolves_to_shared_common_dir() {
	let tmp = TempDir::new().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();

	if !git(&repo, &["init", "-q"]) {
		eprintln!("skipping: git unavailable");
		return;
	}
	fs::write(repo.join("f.txt"), "hi\n").unwrap();
	assert!(git(&repo, &["add", "."]));
	assert!(git(&repo, &["commit", "-q", "-m", "init"]));
	let wt_a = tmp.path().join("wt-a");
	let wt_b = tmp.path().join("wt-b");
	assert!(git(
		&repo,
		&["worktree", "add", "-q", wt_a.to_str().unwrap(), "-b", "a"]
	));
	assert!(git(
		&repo,
		&["worktree", "add", "-q", wt_b.to_str().unwrap(), "-b", "b"]
	));

	let main = discover(&repo).await.unwrap();
	let a = discover(&wt_a).await.unwrap();
	let b = discover(&wt_b).await.unwrap();

	// All three worktrees share one canonical common dir; the linked ones have distinct git dirs.
	assert_eq!(a.common_dir, main.common_dir);
	assert_eq!(b.common_dir, main.common_dir);
	assert_ne!(a.git_dir, b.git_dir);
	assert_eq!(main.common_dir, canon(&repo.join(".git")));
	assert_eq!(a.worktree_root.as_deref(), Some(canon(&wt_a).as_path()));
}

/// `path` as a config value: its backslashes (a Windows path's) escaped, as git's config reads them.
fn config_value(path: &Path) -> String {
	path
		.display()
		.to_string()
		.replace('\\', "\\\\")
		.replace('"', "\\\"")
}

/// A repository whose `.git` directory is at `<base>/repo/.git`, its `config` reading `config`, and an
/// `other` folder beside it.
fn repo_with_config(config: &str) -> (TempDir, PathBuf, PathBuf) {
	let tmp = TempDir::new().unwrap();
	let repo = tmp.path().join("repo");
	ordinary_repo(&repo);
	fs::write(repo.join(".git/config"), config).unwrap();
	let other = tmp.path().join("other");
	fs::create_dir_all(&other).unwrap();
	(tmp, repo, other)
}

/// As git: `core.worktree`, relative to the git directory, puts the work tree elsewhere; the `.git`
/// stays where discovery found it, and re-inspecting that folder gives the same layout. Stock git
/// agrees on the top.
#[tokio::test]
async fn core_worktree_puts_the_work_tree_elsewhere() {
	let (_tmp, repo, other) =
		repo_with_config("[core]\n\trepositoryformatversion = 0\n\tworktree = ../../other\n");
	let layout = discover(&repo).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&other).as_path())
	);
	assert_eq!(layout.marker_root.as_deref(), Some(canon(&repo).as_path()));
	assert_eq!(layout.git_dir, canon(&repo.join(".git")));
	assert_eq!(inspect_root(&repo).await.unwrap(), layout);
	let top = Command::new("git")
		.args(["-C"])
		.arg(&repo)
		.args(["rev-parse", "--show-toplevel"])
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.output()
		.unwrap();
	if top.status.success() {
		assert_eq!(
			PathBuf::from(String::from_utf8_lossy(&top.stdout).trim()),
			canon(&other)
		);
	}
}

#[tokio::test]
async fn an_absolute_core_worktree_is_taken_as_it_is() {
	let tmp = TempDir::new().unwrap();
	let other = tmp.path().join("other");
	fs::create_dir_all(&other).unwrap();
	let (_keep, repo, _) = repo_with_config(&format!(
		"[core]\n\trepositoryformatversion = 0\n\tworktree = {}\n",
		config_value(&canon(&other))
	));
	let layout = discover(&repo).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&other).as_path())
	);
}

/// As git: with `core.bare` true too there is no work tree ("core.bare and core.worktree do not make
/// sense").
#[tokio::test]
async fn core_bare_with_core_worktree_has_no_work_tree() {
	let (_tmp, repo, _) = repo_with_config(
		"[core]\n\trepositoryformatversion = 0\n\tbare = true\n\tworktree = ../../other\n",
	);
	let layout = discover(&repo).await.unwrap();
	assert_eq!(layout.worktree_root, None);
	assert_eq!(layout.marker_root.as_deref(), Some(canon(&repo).as_path()));
	assert!(layout.bare_and_worktree);
}

/// As git: `core.bare` true without `core.worktree` means no work tree too, with nothing to warn of;
/// a config without `core.repositoryformatversion` is set aside, so there the work tree stays.
#[tokio::test]
async fn core_bare_alone_means_no_work_tree() {
	let (_tmp, repo, _) = repo_with_config("[core]\n\trepositoryformatversion = 0\n\tbare = true\n");
	let layout = discover(&repo).await.unwrap();
	assert_eq!(layout.worktree_root, None);
	assert_eq!(layout.marker_root.as_deref(), Some(canon(&repo).as_path()));
	assert!(!layout.bare_and_worktree);
	let (_tmp, repo, _) = repo_with_config("[core]\n\tbare = true\n");
	assert_eq!(
		discover(&repo).await.unwrap().worktree_root.as_deref(),
		Some(canon(&repo).as_path())
	);
}

/// A source read for a local clone needs no work tree: a missing one `core.worktree` names stops a
/// command's discovery but not the source's inspection, which gives its git folders only.
#[tokio::test]
async fn a_source_is_read_without_its_work_tree() {
	let (_tmp, repo, _) =
		repo_with_config("[core]\n\trepositoryformatversion = 0\n\tworktree = ../../nowhere\n");
	assert!(matches!(
		inspect_root(&repo).await,
		Err(DiscoveryError::MissingWorktree { .. })
	));
	let layout = inspect_source(&repo).await.unwrap();
	assert_eq!(layout.worktree_root, None);
	assert_eq!(layout.marker_root.as_deref(), Some(canon(&repo).as_path()));
	assert_eq!(layout.git_dir, canon(&repo.join(".git")));
	assert_eq!(layout.common_dir, canon(&repo.join(".git")));
}

/// A work tree `core.worktree` names that isn't there is an error, not a quiet fallback.
#[tokio::test]
async fn a_missing_core_worktree_is_an_error() {
	let (_tmp, repo, _) =
		repo_with_config("[core]\n\trepositoryformatversion = 0\n\tworktree = ../../nowhere\n");
	assert!(matches!(
		discover(&repo).await,
		Err(DiscoveryError::MissingWorktree { .. })
	));
}

/// An empty `core.worktree` names no folder: refused as a missing work tree, as git refuses it
/// ("cannot chdir to ''").
#[tokio::test]
async fn an_empty_core_worktree_is_refused() {
	let (_tmp, repo, _) = repo_with_config("[core]\n\trepositoryformatversion = 0\n\tworktree =\n");
	assert!(matches!(
		discover(&repo).await,
		Err(DiscoveryError::MissingWorktree { .. })
	));
}

/// A `core.worktree` naming the git folder itself, or a folder inside it, is refused (git accepts
/// it, and its commands then take the repository's own files for work-tree files). A source read
/// for a local clone doesn't look at its work tree, so it's still read.
#[tokio::test]
async fn a_core_worktree_in_the_git_folder_is_refused() {
	for named in [".", "../.git", "refs"] {
		let (_tmp, repo, _) = repo_with_config(&format!(
			"[core]\n\trepositoryformatversion = 0\n\tworktree = {named}\n"
		));
		assert!(
			matches!(
				discover(&repo).await,
				Err(DiscoveryError::WorktreeInGitDir { .. })
			),
			"core.worktree = {named}"
		);
		assert!(
			inspect_source(&repo).await.is_ok(),
			"core.worktree = {named}"
		);
	}
}

/// Found as itself (from inside the `.git`), the work tree is still `core.worktree`'s, as git's
/// `--show-toplevel` from there says; there's no `.git` folder for it.
#[tokio::test]
async fn core_worktree_holds_from_inside_the_git_directory() {
	let (_tmp, repo, other) =
		repo_with_config("[core]\n\trepositoryformatversion = 0\n\tworktree = ../../other\n");
	let layout = discover(&repo.join(".git")).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&other).as_path())
	);
	assert_eq!(layout.marker_root, None);
}

/// As git: a linked worktree ignores the shared `core.worktree`, which is the main checkout's.
#[tokio::test]
async fn a_linked_worktree_ignores_the_main_core_worktree() {
	let (tmp, worktree, _admin, main_git) = hand_built_linked_worktree(true, false);
	let elsewhere = tmp.path().join("elsewhere");
	fs::create_dir_all(&elsewhere).unwrap();
	fs::write(
		main_git.join("config"),
		format!(
			"[core]\n\trepositoryformatversion = 0\n\tworktree = {}\n",
			canon(&elsewhere).display()
		),
	)
	.unwrap();
	let layout = discover(&worktree).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&worktree).as_path())
	);
	assert_eq!(
		layout.marker_root.as_deref(),
		Some(canon(&worktree).as_path())
	);
}

/// As git: a config that doesn't say its `core.repositoryformatversion` is set aside, its
/// `core.worktree` with it (measured: git's top is then the folder holding `.git`).
#[tokio::test]
async fn core_worktree_counts_only_with_a_format_version() {
	let tmp = TempDir::new().unwrap();
	let repo = tmp.path().join("repo");
	ordinary_repo(&repo);
	fs::create_dir_all(tmp.path().join("other")).unwrap();
	fs::write(
		repo.join(".git/config"),
		"[core]\n\tworktree = ../../other\n",
	)
	.unwrap();
	let layout = discover(&repo).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&repo).as_path())
	);
}

/// As git: with `extensions.worktreeConfig`, the checkout's own `config.worktree` overrides the shared
/// `core.worktree` (measured: shared `../../other`, own `..`, git's top is the checkout).
#[tokio::test]
async fn config_worktree_overrides_the_shared_core_worktree() {
	let (_tmp, repo, _other) = repo_with_config(
		"[core]\n\trepositoryformatversion = 1\n\tworktree = ../../other\n[extensions]\n\tworktreeConfig = true\n",
	);
	fs::write(
		repo.join(".git/config.worktree"),
		"[core]\n\tworktree = ..\n",
	)
	.unwrap();
	let layout = discover(&repo).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&repo).as_path())
	);
	let top = Command::new("git")
		.args(["-C"])
		.arg(&repo)
		.args(["rev-parse", "--show-toplevel"])
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.output()
		.unwrap();
	if top.status.success() {
		assert_eq!(
			PathBuf::from(String::from_utf8_lossy(&top.stdout).trim()),
			canon(&repo)
		);
	}
}

/// A config value that isn't UTF-8 (git's config is bytes) doesn't stop discovery.
#[tokio::test]
async fn a_config_value_that_isn_t_utf8_doesn_t_stop_discovery() {
	let tmp = TempDir::new().unwrap();
	let repo = tmp.path().join("repo");
	ordinary_repo(&repo);
	fs::write(
		repo.join(".git/config"),
		b"[core]\n\trepositoryformatversion = 0\n[user]\n\tname = caf\xe9\n",
	)
	.unwrap();
	let layout = discover(&repo).await.unwrap();
	assert_eq!(
		layout.worktree_root.as_deref(),
		Some(canon(&repo).as_path())
	);
}
