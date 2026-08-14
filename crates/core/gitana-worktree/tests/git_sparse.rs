//! Sparse-checkout reapply, oracle-checked against stock git.
//!
//! Each test builds a full checkout, enables cone sparse-checkout *without applying it* (writing the
//! per-worktree config and `.git/info/sparse-checkout` by hand, leaving every file on disk), then runs
//! gitana's `reapply_sparse` and asserts the resulting working tree and index match what git produces —
//! read back through `git ls-files -t` (git reads gitana's skip-worktree bits: `H` = present, `S` =
//! skip-worktree). SHA-256, gated on a git that supports it.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::{ObjectId, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{SparseSet, WorkTree, WorktreeError};

fn open_dir(path: impl AsRef<Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn make_repo(work: &Path) -> WorkTree<LocalFileStore, CapWorkDir, Sha256> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(work)), git_dir)
}

/// A full checkout of `root.txt`, `a/f`, `a/b/g`, `x/h`, then cone sparse-checkout enabled for `a`
/// **without applying** — the whole tree is still on disk, no skip-worktree bits set. Returns the work
/// dir. (So `a/*` and `root.txt` are in the cone; `x/*` is outside it.)
fn full_checkout_with_pending_cone(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("root.txt"), b"r\n").unwrap();
	std::fs::create_dir_all(work.join("a/b")).unwrap();
	std::fs::write(work.join("a/f"), b"af\n").unwrap();
	std::fs::write(work.join("a/b/g"), b"abg\n").unwrap();
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"xh\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "init");

	// Enable cone sparse-checkout the way git stores it (per-worktree), but do NOT run
	// `git sparse-checkout` — so git has not yet removed anything; gitana's reapply must.
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckoutCone",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/*/\n/a/\n").unwrap();
	work
}

/// The `git ls-files -t` status letter (`H`/`S`/…) for `path`.
fn status_of(ls_files_t: &str, path: &str) -> char {
	ls_files_t
		.lines()
		.find(|line| line.get(2..) == Some(path))
		.unwrap_or_else(|| panic!("no `ls-files -t` entry for {path} in:\n{ls_files_t}"))
		.chars()
		.next()
		.unwrap()
}

#[tokio::test]
async fn reapply_cone_removes_excluded_and_sets_skip_worktree_like_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout_with_pending_cone("sparse-cone");
	let w = work.to_str().unwrap();

	let outcome = make_repo(&work).reapply_sparse().await.unwrap();
	assert!(
		outcome.left_dirty.is_empty(),
		"nothing was dirty: {:?}",
		outcome.left_dirty
	);

	// Included files stay on disk; the excluded `x/` subtree is removed.
	assert!(work.join("root.txt").exists());
	assert!(work.join("a/f").exists());
	assert!(work.join("a/b/g").exists());
	assert!(!work.join("x/h").exists(), "excluded x/h should be removed");
	assert!(!work.join("x").exists(), "emptied x/ should be pruned");

	// git reads the index gitana wrote: skip-worktree (`S`) on the excluded file, `H` on the rest.
	let t = git(&["-C", w, "ls-files", "-t"]);
	assert_eq!(status_of(&t, "root.txt"), 'H');
	assert_eq!(status_of(&t, "a/f"), 'H');
	assert_eq!(status_of(&t, "a/b/g"), 'H');
	assert_eq!(status_of(&t, "x/h"), 'S');
}

#[tokio::test]
async fn reapply_leaves_a_locally_modified_excluded_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout_with_pending_cone("sparse-dirty");
	let w = work.to_str().unwrap();
	// Modify the excluded file before reapply — git leaves such a file, warns, and does not set its bit.
	std::fs::write(work.join("x/h"), b"xh LOCALLY MODIFIED\n").unwrap();

	let outcome = make_repo(&work).reapply_sparse().await.unwrap();

	// The modification is preserved (no data loss) and the path is reported as left-behind.
	assert!(
		work.join("x/h").exists(),
		"a modified excluded file is kept"
	);
	assert_eq!(
		std::fs::read(work.join("x/h")).unwrap(),
		b"xh LOCALLY MODIFIED\n"
	);
	assert_eq!(outcome.left_dirty, vec!["x/h".to_owned()]);
	// Its skip-worktree bit was NOT set — it stays a normal tracked file (`H`), as git leaves it.
	let t = git(&["-C", w, "ls-files", "-t"]);
	assert_eq!(status_of(&t, "x/h"), 'H');
	// The clean, included files still applied normally.
	assert!(work.join("a/f").exists());
	assert_eq!(status_of(&t, "a/f"), 'H');
}

/// A sparse repo (cone `a`, excluding `x/`) with two branches whose excluded file differs: `x/h` is
/// `xh\n` on the base branch and `xh2\n` on `other`. Reapply has already run, so on the base branch
/// `x/h` is skip-worktree and absent. Returns `(work, base_tree_hex, other_tree_hex)`.
async fn sparse_two_branches(tag: &str) -> (PathBuf, String, String) {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("root.txt"), b"r\n").unwrap();
	std::fs::create_dir_all(work.join("a")).unwrap();
	std::fs::write(work.join("a/f"), b"af\n").unwrap();
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"xh\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");
	let base_branch = git(&["-C", w, "branch", "--show-current"])
		.trim()
		.to_owned();
	let base_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();

	// A second branch that changes the (soon-to-be) excluded file, so a checkout/reset to it exercises
	// updating a sparse entry's staged content without materialising the file.
	git(&["-C", w, "checkout", "-q", "-b", "other"]);
	std::fs::write(work.join("x/h"), b"xh2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "other");
	let other_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "checkout", "-q", &base_branch]);

	// Enable cone sparse-checkout (per-worktree, as git stores it) for `a`, then apply it via gitana.
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckoutCone",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/*/\n/a/\n").unwrap();
	make_repo(&work).reapply_sparse().await.unwrap();
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'S');
	assert!(!work.join("x/h").exists());
	(work, base_tree, other_tree)
}

/// Switching to a branch where the *excluded* file differs updates its staged content but keeps it
/// skip-worktree and unmaterialised — git preserves the bit and never populates a sparse path.
#[tokio::test]
async fn checkout_preserves_skip_worktree_and_does_not_materialise() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _base, other) = sparse_two_branches("sparse-checkout-switch").await;
	let w = work.to_str().unwrap();

	make_repo(&work)
		.checkout(ObjectId::<Sha256>::from_hex(&other).unwrap(), false, None)
		.await
		.unwrap();

	let t = git(&["-C", w, "ls-files", "-t"]);
	assert_eq!(
		status_of(&t, "x/h"),
		'S',
		"excluded path stays skip-worktree"
	);
	assert!(
		!work.join("x/h").exists(),
		"a sparse path is never materialised"
	);
	// The index entry was updated to the target branch's content, even though nothing hit the disk.
	assert_eq!(git(&["-C", w, "cat-file", "blob", ":x/h"]), "xh2\n");
	// The included file is unchanged across branches and stays present.
	assert_eq!(status_of(&t, "a/f"), 'H');
	assert!(work.join("a/f").exists());
}

/// A branch switch must refuse when a *recreated* excluded file has local edits: git treats the
/// reappeared file as materialized and refuses to discard the edit by re-omitting the path (probed vs
/// git 2.50.1 — a clean recreation is fine, a dirty one, or one equal to the target, refuses). Without
/// this a dirty checkout would succeed and silently absorb the edit.
#[tokio::test]
async fn checkout_refuses_a_dirty_recreated_excluded_file() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _base, other) = sparse_two_branches("sparse-checkout-dirty").await;
	// Recreate the excluded x/h on disk with local edits (differs from the index's `xh`).
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"LOCAL\n").unwrap();

	let result = make_repo(&work)
		.checkout(ObjectId::<Sha256>::from_hex(&other).unwrap(), false, None)
		.await;
	assert!(
		result.is_err(),
		"a dirty recreated excluded file must refuse the checkout"
	);
	// The local edit is preserved, not absorbed.
	assert_eq!(std::fs::read(work.join("x/h")).unwrap(), b"LOCAL\n");
}

/// A CLEAN recreated excluded file (bytes matching the index) is removed and re-omitted by a non-force
/// branch switch that changes the path — git treats it as reconstructable rather than leaving a spurious
/// modification (probed vs git 2.50.1). Full-parity companion to the dirty-refusal case.
#[tokio::test]
async fn checkout_reomits_a_clean_recreated_excluded_file() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _base, other) = sparse_two_branches("sparse-checkout-clean").await;
	let w = work.to_str().unwrap();
	// Recreate x/h with the SAME bytes as the index (`xh`) — a clean reconstruction.
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"xh\n").unwrap();

	make_repo(&work)
		.checkout(ObjectId::<Sha256>::from_hex(&other).unwrap(), false, None)
		.await
		.unwrap();

	// git removes the reconstructable file and re-omits the path with the target's content.
	assert!(
		!work.join("x/h").exists(),
		"a clean recreated excluded file is removed"
	);
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'S');
	assert_eq!(git(&["-C", w, "cat-file", "blob", ":x/h"]), "xh2\n");
}

/// The fast-forward path enforces the same rule: a dirty recreated excluded file refuses the merge.
#[tokio::test]
async fn fast_forward_refuses_a_dirty_recreated_excluded_file() {
	if !git_supports_sha256() {
		return;
	}
	let (work, base, other) = sparse_two_branches("sparse-ff-dirty").await;
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"LOCAL\n").unwrap();

	let refused = make_repo(&work)
		.twoway_merge(
			ObjectId::<Sha256>::from_hex(&base).unwrap(),
			ObjectId::<Sha256>::from_hex(&other).unwrap(),
		)
		.await
		.unwrap();
	assert_eq!(refused, vec!["x/h".to_owned()], "FF must be refused");
	assert_eq!(std::fs::read(work.join("x/h")).unwrap(), b"LOCAL\n");
}

/// A fast-forward must still refuse when it would discard *divergent staged content* at an excluded
/// path — even though that path is never materialised, `twoway_merge` rewrites its index entry, so
/// applying the update would silently drop the staged blob. git refuses the checkout here (probed and
/// oracled vs stock git 2.50.1); gitana must too, leaving the index untouched.
#[tokio::test]
async fn fast_forward_refuses_divergent_staged_content_at_excluded_path() {
	if !git_supports_sha256() {
		return;
	}
	let (work, base, other) = sparse_two_branches("sparse-ff-staged").await;
	let w = work.to_str().unwrap();

	// Force divergent staged content into the excluded (skip-worktree) `x/h`: differs from HEAD and
	// from the target branch. `git add --sparse` is the only way to stage an out-of-cone path.
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"xh-staged\n").unwrap();
	git(&["-C", w, "add", "--sparse", "x/h"]);
	git(&["-C", w, "update-index", "--skip-worktree", "x/h"]);
	std::fs::remove_dir_all(work.join("x")).unwrap();
	assert_eq!(git(&["-C", w, "cat-file", "blob", ":x/h"]), "xh-staged\n");

	// gitana refuses: the changed path is reported and nothing is applied.
	let refused = make_repo(&work)
		.twoway_merge(
			ObjectId::<Sha256>::from_hex(&base).unwrap(),
			ObjectId::<Sha256>::from_hex(&other).unwrap(),
		)
		.await
		.unwrap();
	assert_eq!(refused, vec!["x/h".to_owned()], "FF must be refused");
	assert_eq!(
		git(&["-C", w, "cat-file", "blob", ":x/h"]),
		"xh-staged\n",
		"staged content must survive a refused fast-forward"
	);

	// Oracle: stock git also refuses to switch branches from this state.
	let out = Command::new("git")
		.args(["-C", w, "checkout", "other"])
		.output()
		.expect("run git checkout");
	assert!(!out.status.success(), "git should refuse the checkout too");
	assert!(
		String::from_utf8_lossy(&out.stderr).contains("would be overwritten"),
		"git refuses to preserve the staged content"
	);
}

/// A checkout that introduces *new* paths under active sparse-checkout recomputes each from the
/// current patterns (git's behaviour): a new excluded path is added skip-worktree and NOT written,
/// a new included path is materialised.
#[tokio::test]
async fn checkout_excludes_new_paths_by_the_sparse_patterns() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("sparse-checkout-new");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("a")).unwrap();
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("a/f"), b"af\n").unwrap();
	std::fs::write(work.join("x/h"), b"xh\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");
	let base_branch = git(&["-C", w, "branch", "--show-current"])
		.trim()
		.to_owned();

	// `other` adds a new included file (a/new) and a new excluded file (x/new).
	git(&["-C", w, "checkout", "-q", "-b", "other"]);
	std::fs::write(work.join("a/new"), b"anew\n").unwrap();
	std::fs::write(work.join("x/new"), b"xnew\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "other");
	let other_tree = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	git(&["-C", w, "checkout", "-q", &base_branch]);

	// Enable non-cone sparse-checkout excluding `x/`, and apply it.
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/x/\n").unwrap();
	make_repo(&work).reapply_sparse().await.unwrap();

	make_repo(&work)
		.checkout(
			ObjectId::<Sha256>::from_hex(&other_tree).unwrap(),
			false,
			None,
		)
		.await
		.unwrap();

	let t = git(&["-C", w, "ls-files", "-t"]);
	// The new included path is materialised; the new excluded path is skip-worktree and absent.
	assert_eq!(status_of(&t, "a/new"), 'H');
	assert!(work.join("a/new").exists());
	assert_eq!(
		status_of(&t, "x/new"),
		'S',
		"a new excluded path is skip-worktree"
	);
	assert!(
		!work.join("x/new").exists(),
		"a new excluded path is not written"
	);
	assert_eq!(status_of(&t, "x/h"), 'S');
}

/// `reset` (the index half of `reset --mixed`) rebuilds the index from a tree; it must carry the
/// skip-worktree bit forward rather than un-sparsing the path, as git does.
#[tokio::test]
async fn reset_index_preserves_skip_worktree() {
	if !git_supports_sha256() {
		return;
	}
	let (work, base, _other) = sparse_two_branches("sparse-reset").await;
	let w = work.to_str().unwrap();

	make_repo(&work)
		.reset_index(ObjectId::<Sha256>::from_hex(&base).unwrap())
		.await
		.unwrap();

	assert_eq!(
		status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"),
		'S',
		"reset must not clear the skip-worktree bit"
	);
	assert!(!work.join("x/h").exists());
}

/// `add .` must not stage the deletion of an excluded path whose file is absent by design — that
/// would drop the entry from the index entirely (data loss). git leaves sparse paths untouched.
#[tokio::test]
async fn add_does_not_delete_a_sparse_entry() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _base, _other) = sparse_two_branches("sparse-add").await;
	let w = work.to_str().unwrap();

	make_repo(&work).add(&["."], "", false, None).await.unwrap();

	// `status_of` panics if `x/h` is gone from the index — so this asserts the entry survived — and
	// it must still be skip-worktree, with its file still absent.
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'S');
	assert!(!work.join("x/h").exists());
}

/// `restore` treats a sparse path as outside the pathspec: a broad `restore .` skips it (leaving it
/// absent and skip-worktree), and an explicit `restore x/h` matches nothing and errors, as in git.
#[tokio::test]
async fn restore_skips_sparse_paths() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _base, _other) = sparse_two_branches("sparse-restore").await;
	let w = work.to_str().unwrap();

	// Broad restore: sparse paths are silently skipped, not materialised.
	make_repo(&work)
		.restore(None, true, false, &["."], "")
		.await
		.unwrap();
	assert!(!work.join("x/h").exists());
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'S');

	// Explicit restore of the sparse path matches nothing (git: "did not match any file(s)").
	let explicit = make_repo(&work)
		.restore(None, true, false, &["x/h"], "")
		.await;
	assert!(
		explicit.is_err(),
		"an explicit restore of a sparse path should not match"
	);
}

/// A skip-worktree path *recreated on disk* is materialized (git clears the bit on reappearance), so
/// `restore .` replaces it from the index rather than silently leaving the local content (probed vs
/// git 2.50.1). Regression: a present omitted path must not be excluded from restore by its bit alone.
#[tokio::test]
async fn restore_replaces_a_present_skip_worktree_path() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _base, _other) = sparse_two_branches("sparse-restore-present").await;
	// x/h is skip-worktree and absent; recreate it on disk with local edits (gitana keeps the bit S).
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"LOCAL-EDIT\n").unwrap();

	make_repo(&work)
		.restore(None, true, false, &["."], "")
		.await
		.unwrap();

	// The present path was selected and restored from the index (committed `xh\n`), discarding the edit.
	assert_eq!(std::fs::read(work.join("x/h")).unwrap(), b"xh\n");
}

/// A full checkout of `root.txt`, `a/f`, `x/h` with NO sparse-checkout configured — the starting
/// point for exercising gitana's own `apply_sparse_set` / `disable_sparse`.
fn full_checkout(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("root.txt"), b"r\n").unwrap();
	std::fs::create_dir_all(work.join("a")).unwrap();
	std::fs::write(work.join("a/f"), b"af\n").unwrap();
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"xh\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");
	work
}

/// `apply_sparse_set` (cone) writes git's config + pattern file and applies it: the excluded subtree is
/// removed and skip-worktree, the included ones stay — read back through stock git.
#[tokio::test]
async fn apply_sparse_set_cone_matches_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-apply-cone");
	let w = work.to_str().unwrap();

	let outcome = make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	assert!(outcome.left_dirty.is_empty());

	// The on-disk pattern file is git's exact cone byte format.
	assert_eq!(
		std::fs::read_to_string(work.join(".git/info/sparse-checkout")).unwrap(),
		"/*\n!/*/\n/a/\n"
	);
	// git parses the config gitana wrote: sparse enabled, cone mode, via the worktree-config extension.
	assert_eq!(
		git(&["-C", w, "config", "--get", "extensions.worktreeConfig"]).trim(),
		"true"
	);
	assert_eq!(
		git(&["-C", w, "config", "--get", "core.sparseCheckout"]).trim(),
		"true"
	);
	assert_eq!(
		git(&["-C", w, "config", "--get", "core.sparseCheckoutCone"]).trim(),
		"true"
	);

	// The excluded subtree is gone and skip-worktree; the included ones remain.
	assert!(!work.join("x/h").exists());
	assert!(work.join("a/f").exists());
	let t = git(&["-C", w, "ls-files", "-t"]);
	assert_eq!(status_of(&t, "a/f"), 'H');
	assert_eq!(status_of(&t, "x/h"), 'S');
	assert_eq!(status_of(&t, "root.txt"), 'H');
}

/// With `core.ignoreCase=true` (git's default on macOS/Windows), sparse patterns match
/// case-insensitively: a cone `Dir` includes the index path `dir/f`. Proves the config is read and
/// threaded into the matcher end-to-end; probed + oracled vs git 2.50.1.
#[tokio::test]
async fn cone_matches_case_insensitively_under_ignorecase() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("sparse-ignorecase");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("dir")).unwrap();
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("dir/f"), b"1\n").unwrap();
	std::fs::write(work.join("sub/g"), b"2\n").unwrap();
	std::fs::write(work.join("Root.txt"), b"r\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");
	git(&["-C", w, "config", "core.ignoreCase", "true"]);

	// Cone `set Dir` (capitalised) against the lowercase index path.
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["Dir".to_owned()]))
		.await
		.unwrap();

	// `Dir` folds to `dir`, so `dir/f` is included (`H`, on disk); `sub/g` stays excluded (`S`).
	let t = git(&["-C", w, "ls-files", "-t"]);
	assert_eq!(status_of(&t, "dir/f"), 'H', "Dir folds to dir");
	assert_eq!(status_of(&t, "sub/g"), 'S');
	assert_eq!(status_of(&t, "Root.txt"), 'H');
	assert!(work.join("dir/f").exists());
	assert!(!work.join("sub/g").exists());
}

/// `current_sparse_set` recovers the configured directories (git's cone `list`).
#[tokio::test]
async fn current_sparse_set_lists_cone_dirs() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-list");
	let wt = make_repo(&work);
	wt.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();

	assert_eq!(
		make_repo(&work).current_sparse_set().await.unwrap(),
		Some(SparseSet::Cone(vec!["a".to_owned()]))
	);
}

/// `disable_sparse` materialises every omitted file, clears the bits, and records git's disabled config,
/// while leaving the pattern file in place — matching `git sparse-checkout disable`.
#[tokio::test]
async fn disable_sparse_materialises_and_matches_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-disable");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	assert!(!work.join("x/h").exists());

	make_repo(&work).disable_sparse().await.unwrap();

	// Every omitted file is materialised and no longer skip-worktree.
	assert!(work.join("x/h").exists());
	assert_eq!(std::fs::read(work.join("x/h")).unwrap(), b"xh\n");
	let t = git(&["-C", w, "ls-files", "-t"]);
	assert_eq!(status_of(&t, "x/h"), 'H');
	// git reads the disabled config; the pattern file is left in place, as git leaves it.
	assert_eq!(
		git(&["-C", w, "config", "--get", "core.sparseCheckout"]).trim(),
		"false"
	);
	assert!(work.join(".git/info/sparse-checkout").exists());
}

/// Re-including a path whose materialisation is blocked by an untracked file at an ancestor slot must
/// NOT delete that file. git preserves it, clears the path's skip-worktree bit but writes nothing, and
/// reports it "not updated"; the path then shows as deleted in `status`. Regression for the
/// `ensure_parents` ancestor-deletion data-loss path (probed + oracled vs git 2.50.1).
#[tokio::test]
async fn widening_preserves_an_untracked_file_at_an_ancestor_slot() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-widen-anc");
	let w = work.to_str().unwrap();
	// Exclude `x/` (skip-worktree; the emptied `x/` is pruned), then drop an untracked regular file
	// into the freed `x` slot — occupying the parent that materialising `x/h` would need.
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	assert!(!work.join("x").exists());
	std::fs::write(work.join("x"), b"untracked\n").unwrap();

	// Widen to include `x`: `x/h` cannot be materialised (the untracked file is in the way).
	let outcome = make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned(), "x".to_owned()]))
		.await
		.unwrap();
	assert_eq!(outcome.not_updated, vec!["x/h".to_owned()]);

	// The untracked file survives untouched.
	assert!(work.join("x").is_file(), "untracked file must be preserved");
	assert_eq!(std::fs::read(work.join("x")).unwrap(), b"untracked\n");
	// git agrees: the bit is cleared (`H`) but nothing was written, so `x/h` reads as deleted and `x`
	// as untracked.
	let t = git(&["-C", w, "ls-files", "-t"]);
	assert_eq!(status_of(&t, "x/h"), 'H');
	assert!(!work.join("x/h").exists());
	let porcelain = git(&["-C", w, "status", "--porcelain"]);
	assert!(
		porcelain.contains(" D x/h"),
		"x/h shows deleted: {porcelain}"
	);
	assert!(
		porcelain.contains("?? x\n"),
		"x shows untracked: {porcelain}"
	);
}

/// A symlink at an ancestor slot blocks materialisation exactly like a regular file: git preserves the
/// symlink, reports the path "not updated", and succeeds (probed vs git 2.50.1). gitana must not fall
/// through to the `UnsafePath` error, which would abort after the new config/patterns were persisted.
#[tokio::test]
async fn widening_preserves_an_untracked_symlink_at_an_ancestor_slot() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-widen-symlink");
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	assert!(!work.join("x").exists());
	// Drop an untracked (dangling, work-tree-relative) symlink into the freed `x` slot.
	std::os::unix::fs::symlink("gone", work.join("x")).unwrap();

	let outcome = make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned(), "x".to_owned()]))
		.await
		.unwrap();
	assert_eq!(outcome.not_updated, vec!["x/h".to_owned()]);
	// The symlink survives untouched (it was never traversed or deleted).
	assert!(
		work.join("x").symlink_metadata().unwrap().is_symlink(),
		"the untracked symlink must be preserved"
	);
	assert!(!work.join("x/h").exists());
}

/// An *escaping* symlink (pointing outside the capability root) at an ancestor slot must not abort the
/// operation: the descendant is unreachable from the sandbox, so it is treated as absent and the
/// symlink is preserved and reported "not updated" like an in-tree one (git ignores the omitted child).
#[tokio::test]
async fn widening_tolerates_an_escaping_symlink_ancestor() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-widen-escsym");
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	assert!(!work.join("x").exists());
	// A symlink pointing OUTSIDE the work tree (an absolute path) — an lstat through it escapes the
	// cap-std sandbox and previously aborted the reapply.
	std::os::unix::fs::symlink("/tmp", work.join("x")).unwrap();

	let outcome = make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned(), "x".to_owned()]))
		.await
		.unwrap();
	assert_eq!(outcome.not_updated, vec!["x/h".to_owned()]);
	assert!(
		work.join("x").symlink_metadata().unwrap().is_symlink(),
		"the escaping symlink is preserved"
	);
}

/// `disable` materialises every omitted path, but an untracked file at an ancestor slot blocks one —
/// git preserves that file and reports the path "not updated" rather than destroying it.
#[tokio::test]
async fn disable_preserves_an_untracked_file_at_an_ancestor_slot() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-disable-anc");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	std::fs::write(work.join("x"), b"untracked\n").unwrap();

	let outcome = make_repo(&work).disable_sparse().await.unwrap();
	assert_eq!(outcome.not_updated, vec!["x/h".to_owned()]);
	assert!(outcome.left_dirty.is_empty());

	assert_eq!(std::fs::read(work.join("x")).unwrap(), b"untracked\n");
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'H');
	assert!(!work.join("x/h").exists());
}

/// Widening the sparse set over a path where the user has since placed a *modified* file must preserve
/// the local bytes (git keeps it, clears the bit, reports it modified) — never overwrite with the
/// indexed blob. Regression for the materialise data-loss path.
#[tokio::test]
async fn widening_preserves_a_present_modified_excluded_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-widen-preserve");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	assert!(!work.join("x/h").exists());

	// The user recreates the excluded file on disk with different content, then widens to include it.
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"LOCAL EDIT\n").unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned(), "x".to_owned()]))
		.await
		.unwrap();

	// The local edit survives; the bit is cleared and git reports the file modified.
	assert_eq!(std::fs::read(work.join("x/h")).unwrap(), b"LOCAL EDIT\n");
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'H');
	let status = git(&["-C", w, "status", "--short"]);
	assert!(
		status.contains("x/h"),
		"the preserved modified file must be reported: {status}"
	);
}

/// `add` must not stage a *new* untracked file that is outside the active sparse cone. git stages the
/// in-cone work, writes the index, and THEN exits nonzero with its "outside sparse-checkout" advice when
/// a broad pathspec swept up an untracked out-of-cone file (probed vs git 2.50.1) — so `add .` here
/// returns the deferred `PathspecAdvisory` while leaving `x/new` unstaged.
#[tokio::test]
async fn add_refuses_a_new_out_of_cone_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-add-refuse");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();

	// A brand-new untracked file outside the cone.
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/new"), b"n\n").unwrap();
	let result = make_repo(&work).add(&["."], "", false, None).await;
	assert!(
		matches!(&result, Err(WorktreeError::PathspecAdvisory { sparse, .. }) if !sparse.is_empty()),
		"a new out-of-cone file must yield the deferred sparse error, got {result:?}"
	);

	assert!(
		!git(&["-C", w, "ls-files"])
			.lines()
			.any(|line| line == "x/new"),
		"a new out-of-cone file must not be staged by add"
	);
}

/// A path-limited `reset` (`reset_index_paths`) of an explicitly named sparse path updates its staged
/// blob while keeping the skip-worktree bit and leaving the working tree absent — git's behaviour, and
/// distinct from `restore`, which skips sparse paths entirely.
#[tokio::test]
async fn reset_paths_updates_a_sparse_entry_keeping_the_bit() {
	if !git_supports_sha256() {
		return;
	}
	let (work, _base, other) = sparse_two_branches("sparse-reset-path").await;
	let w = work.to_str().unwrap();

	make_repo(&work)
		.reset_index_paths(ObjectId::<Sha256>::from_hex(&other).unwrap(), &["x/h"], "")
		.await
		.unwrap();

	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'S');
	assert_eq!(git(&["-C", w, "cat-file", "blob", ":x/h"]), "xh2\n");
	assert!(!work.join("x/h").exists());
}

/// `reapply` reconciles a file recreated at an already-omitted (skip-worktree) path: git clears the
/// bit and reports it left despite the patterns, rather than continuing to hide it.
#[tokio::test]
async fn reapply_reconciles_a_recreated_omitted_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-reapply-present");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	assert!(!work.join("x/h").exists());

	// Recreate the omitted file on disk (bit still set), then reapply directly.
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"RESURRECTED\n").unwrap();
	let outcome = make_repo(&work).reapply_sparse().await.unwrap();

	assert_eq!(outcome.left_dirty, vec!["x/h".to_owned()]);
	assert_eq!(std::fs::read(work.join("x/h")).unwrap(), b"RESURRECTED\n");
	// The bit is cleared, so git reports the present file rather than hiding it.
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'H');
}

/// An explicit `add` of an out-of-cone path is refused; a broad `add .` silently skips it (already
/// covered), so this pins the explicit-refusal half.
#[tokio::test]
async fn explicit_add_of_an_out_of_cone_path_is_refused() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-add-explicit");
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	// A new out-of-cone file, named explicitly.
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/new"), b"n\n").unwrap();
	let result = make_repo(&work).add(&["x/new"], "", false, None).await;
	assert!(
		result.is_err(),
		"explicit add of an out-of-cone path must error"
	);
}

/// An explicitly-named out-of-cone *directory* is refused too (git reports it rather than staging
/// nothing) — whether it is absent from the working tree or recreated on disk. Probed vs git 2.50.1.
#[tokio::test]
async fn explicit_add_of_an_out_of_cone_directory_is_refused() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-add-dir");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();

	// (d) the excluded directory is absent from the working tree.
	assert!(!work.join("x").exists());
	assert!(
		make_repo(&work).add(&["x"], "", false, None).await.is_err(),
		"add of an absent out-of-cone directory must error"
	);
	// (g) recreate it on disk with content — still refused.
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"X\n").unwrap();
	assert!(
		make_repo(&work).add(&["x"], "", false, None).await.is_err(),
		"add of a present out-of-cone directory must error"
	);
	// Nothing was staged: the index keeps x/h's original content, not the recreated bytes. (git also
	// clears the skip-worktree bit when the file reappears, a separate concern from refusing the add.)
	assert_eq!(git(&["-C", w, "cat-file", "blob", ":x/h"]), "xh\n");
}

/// An in-cone directory `add` stages its content (it is not refused) — the guard must not over-reject.
#[tokio::test]
async fn add_of_an_in_cone_directory_stages_it() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-add-incone-dir");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	std::fs::write(work.join("a/f"), b"CHANGED\n").unwrap();

	make_repo(&work).add(&["a"], "", false, None).await.unwrap();
	assert_eq!(git(&["-C", w, "cat-file", "blob", ":a/f"]), "CHANGED\n");
}

/// A non-cone directory with *mixed* content (one included file, one excluded) is not refused: `add`
/// stages the in-cone file and silently skips the out-of-cone one, as git does (probed vs git 2.50.1).
#[tokio::test]
async fn add_of_a_mixed_noncone_directory_stages_only_in_cone() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("sparse-add-mixed");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("d")).unwrap();
	std::fs::write(work.join("d/in"), b"i\n").unwrap();
	std::fs::write(work.join("d/out"), b"o\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");
	// Non-cone: include only `d/in`.
	git(&["-C", w, "config", "extensions.worktreeConfig", "true"]);
	git(&[
		"-C",
		w,
		"config",
		"--worktree",
		"core.sparseCheckout",
		"true",
	]);
	std::fs::write(work.join(".git/info/sparse-checkout"), "/*\n!/*/\n/d/in\n").unwrap();
	make_repo(&work).reapply_sparse().await.unwrap();
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "d/out"), 'S');

	std::fs::write(work.join("d/in"), b"CHANGED\n").unwrap();
	make_repo(&work).add(&["d"], "", false, None).await.unwrap();
	assert_eq!(git(&["-C", w, "cat-file", "blob", ":d/in"]), "CHANGED\n");
	// The excluded sibling stays out-of-cone and unstaged.
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "d/out"), 'S');
}

/// After reapply leaves a *modified* out-of-cone file (its skip-worktree bit cleared), an explicit
/// `add` of the containing directory is STILL refused — a covered entry is classified by the matcher,
/// not the bit (probed vs git 2.50.1). Regression for the bit-vs-matcher misclassification.
#[tokio::test]
async fn add_of_a_directory_with_only_a_dirty_excluded_file_is_refused() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-add-dirty-excluded");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	// Recreate + modify the excluded file; reapply leaves it (bit cleared, present).
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"MODIFIED\n").unwrap();
	let outcome = make_repo(&work).reapply_sparse().await.unwrap();
	assert_eq!(outcome.left_dirty, vec!["x/h".to_owned()]);
	assert_eq!(
		status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"),
		'H',
		"reapply cleared the bit on the dirty excluded file"
	);

	// `add x` is still refused (x is out-of-cone per the matcher), and the modification is not staged.
	assert!(
		make_repo(&work).add(&["x"], "", false, None).await.is_err(),
		"add of a dir with only a dirty excluded file must still error"
	);
	assert_eq!(git(&["-C", w, "cat-file", "blob", ":x/h"]), "xh\n");
}

/// A FORCE checkout (`reset --hard`, an abort, `checkout -f`) over a recreated excluded file REMOVES it
/// and restores the skip-worktree omission — unlike a non-force branch switch, which preserves it.
#[tokio::test]
async fn force_checkout_removes_a_recreated_excluded_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-force-checkout");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	// Recreate the omitted file, then force-checkout HEAD (git's reset --hard materialises through this).
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"LOCAL\n").unwrap();
	let head = git(&["-C", w, "rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	make_repo(&work)
		.checkout(ObjectId::<Sha256>::from_hex(&head).unwrap(), true, None)
		.await
		.unwrap();

	assert!(
		!work.join("x/h").exists(),
		"a force checkout discards the recreated excluded file"
	);
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'S');
}

/// `mv` refuses a destination outside the sparse-checkout definition, as git does — moving an in-cone
/// path out of cone would land out-of-cone content in the index without `--sparse` (probed vs git 2.50.1).
#[tokio::test]
async fn mv_refuses_an_out_of_cone_destination() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-mv-dest");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();

	// a/f is in-cone (present); moving it to the excluded `x/` is refused.
	let result = make_repo(&work).mv(&["a/f"], "x/f", "", false, false).await;
	assert!(
		result.is_err(),
		"moving an in-cone path out of cone must be refused"
	);
	// a/f is untouched; nothing landed at the out-of-cone destination.
	assert!(work.join("a/f").exists());
	let listed = git(&["-C", w, "ls-files"]);
	assert!(listed.contains("a/f"));
	assert!(!listed.contains("x/f"));
}

/// A directory `mv` is refused when its remapped *children* land out of cone, even though the
/// destination name itself looks like an included root entry — git names the child paths (probed vs git
/// 2.50.1). Regression for validating `dst/rest`, not just `dst`.
#[tokio::test]
async fn mv_refuses_a_directory_whose_children_leave_the_cone() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("sparse-mv-dir");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("a/sub")).unwrap();
	std::fs::write(work.join("a/sub/f"), b"f\n").unwrap();
	std::fs::write(work.join("root.txt"), b"r\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();

	// Move the in-cone dir `a/sub` to root-level `b`: `b` looks like an included root entry, but the
	// resulting `b/f` is out of cone, so git refuses.
	let result = make_repo(&work).mv(&["a/sub"], "b", "", false, false).await;
	assert!(
		result.is_err(),
		"a dir move whose children leave the cone must be refused"
	);
	assert!(work.join("a/sub/f").exists());
	let listed = git(&["-C", w, "ls-files"]);
	assert!(
		!listed.lines().any(|line| line == "b/f"),
		"nothing landed at the out-of-cone destination: {listed}"
	);
}

/// `rm` does not remove a sparse (out-of-cone) entry: a broad `rm -r .` leaves omitted paths in the
/// index rather than staging their deletion.
#[tokio::test]
async fn rm_leaves_sparse_entries() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-rm");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();

	make_repo(&work)
		.rm(&["."], "", false, false, true, false)
		.await
		.unwrap();

	// The omitted x/h entry survives (still tracked, still skip-worktree).
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'S');
}

/// `rm -r .` must also preserve a *modified* out-of-cone file whose skip-worktree bit reapply cleared:
/// it is still outside the sparse definition, so git leaves it (probed vs git 2.50.1). Regression for
/// an rm data-loss where the bit-only filter deleted and staged the out-of-cone content.
#[tokio::test]
async fn rm_preserves_a_dirty_excluded_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-rm-dirty");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::Cone(vec!["a".to_owned()]))
		.await
		.unwrap();
	// Recreate + modify the excluded file; reapply leaves it (bit cleared, present).
	std::fs::create_dir_all(work.join("x")).unwrap();
	std::fs::write(work.join("x/h"), b"MODIFIED\n").unwrap();
	make_repo(&work).reapply_sparse().await.unwrap();
	assert_eq!(status_of(&git(&["-C", w, "ls-files", "-t"]), "x/h"), 'H');

	// A broad `rm -r .` removes only the in-cone paths; the dirty out-of-cone file is preserved.
	let outcome = make_repo(&work)
		.rm(&["."], "", false, false, true, false)
		.await
		.unwrap();
	assert_eq!(
		outcome.removed,
		vec!["a/f".to_owned(), "root.txt".to_owned()]
	);
	assert!(
		work.join("x/h").exists(),
		"the dirty excluded file must not be deleted"
	);
	assert_eq!(std::fs::read(work.join("x/h")).unwrap(), b"MODIFIED\n");
	assert!(
		git(&["-C", w, "ls-files"]).contains("x/h"),
		"x/h stays tracked"
	);
}

/// `NonCone(["/*", "!/*/"])` — git's non-cone root-only encoding — really omits every subdirectory,
/// leaving only root files (so `init --no-cone` sparsifies like the cone default).
#[tokio::test]
async fn non_cone_root_only_omits_subdirectories() {
	if !git_supports_sha256() {
		return;
	}
	let work = full_checkout("sparse-noncone-root");
	let w = work.to_str().unwrap();
	make_repo(&work)
		.apply_sparse_set(&SparseSet::NonCone(vec![
			"/*".to_owned(),
			"!/*/".to_owned(),
		]))
		.await
		.unwrap();

	assert!(work.join("root.txt").exists());
	assert!(!work.join("a").exists(), "a/ subtree omitted");
	assert!(!work.join("x").exists(), "x/ subtree omitted");
	let t = git(&["-C", w, "ls-files", "-t"]);
	assert_eq!(status_of(&t, "root.txt"), 'H');
	assert_eq!(status_of(&t, "a/f"), 'S');
	assert_eq!(status_of(&t, "x/h"), 'S');
}

// --- helpers (self-contained per test binary, matching the other git_*.rs tests) ---

fn commit(work: &str, msg: &str) {
	git(&[
		"-C",
		work,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		msg,
	]);
}

fn git(args: &[&str]) -> String {
	let out = Command::new("git").args(args).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!(
		"gitana-worktree-{tag}-{}-{seq}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-sparse");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
