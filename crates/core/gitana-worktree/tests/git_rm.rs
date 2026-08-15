#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::Sha256;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{IndexEntry, Stat, WorkTree, WorktreeError};

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn make_repo(work: &std::path::Path) -> WorkTree<LocalFileStore, CapWorkDir, Sha256> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(work)), git_dir)
}

#[tokio::test]
async fn rm_rejects_unsafe_index_path() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("rm-unsafe");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "one");
	let wt = make_repo(&work);

	// Inject a hostile entry escaping the work tree, as a corrupt/hostile index might carry.
	let name = format!("{}-escape.txt", work.file_name().unwrap().to_string_lossy());
	let outside = work.parent().unwrap().join(&name);
	let _ = std::fs::remove_file(&outside);
	std::fs::write(&outside, b"VICTIM\n").unwrap();

	let mut index = wt.load_index().await.unwrap();
	let blob = wt.repository().write_blob(b"PWN\n").await.unwrap();
	index.upsert(IndexEntry {
		stat: Stat::default(),
		mode: 0o100644,
		oid: blob,
		stage: 0,
		assume_valid: false,
		skip_worktree: false,
		intent_to_add: false,
		path: format!("../{name}"),
	});
	wt.save_index(&index).await.unwrap();

	// `rm -r .` selects every tracked path; the escaping one must be rejected before any
	// working-tree file is deleted.
	assert!(matches!(
		wt.rm(&["."], "", false, true, true, false).await,
		Err(WorktreeError::UnsafePath(_))
	));
	assert!(
		outside.exists(),
		"the file outside the work tree is untouched"
	);
	assert_eq!(std::fs::read(&outside).unwrap(), b"VICTIM\n");

	let _ = std::fs::remove_file(&outside);
	std::fs::remove_dir_all(&work).ok();
}

/// `rm` matches a glob pathspec the git way — `*` crosses `/`, so `*.rs` removes every `.rs` at any
/// depth — and a glob never requires `-r` (probed vs git 2.50.1).
#[tokio::test]
async fn rm_matches_a_glob_across_directories() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-glob");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("src/sub")).unwrap();
	std::fs::write(work.join("src/a.rs"), b"1\n").unwrap();
	std::fs::write(work.join("src/sub/b.rs"), b"2\n").unwrap();
	std::fs::write(work.join("src/c.txt"), b"3\n").unwrap();
	std::fs::write(work.join("top.rs"), b"4\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	// `*.rs` with NO `-r` (recursive=false) removes all `.rs` at any depth; `.txt` is left.
	let outcome = make_repo(&work)
		.rm(&["*.rs"], "", false, false, false, false)
		.await
		.unwrap();
	let mut removed = outcome.removed.clone();
	removed.sort();
	assert_eq!(
		removed,
		vec![
			"src/a.rs".to_owned(),
			"src/sub/b.rs".to_owned(),
			"top.rs".to_owned()
		]
	);
	// Oracle: git reads the resulting index — only the non-matching `.txt` survives.
	assert_eq!(
		git(&["-C", w, "ls-files"]).lines().collect::<Vec<_>>(),
		vec!["src/c.txt"]
	);
	assert!(!work.join("src/a.rs").exists() && !work.join("top.rs").exists());

	std::fs::remove_dir_all(&work).ok();
}

/// `rm -r a ':!a'` is a no-op success: git decides the positive `a` matched *before* subtracting the
/// exclusion, so nothing is removed and no "did not match" error is raised (probed vs git 2.50.1).
#[tokio::test]
async fn rm_positive_then_excluded_is_a_noop() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-exclude-noop");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a"), b"1\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	let outcome = make_repo(&work)
		.rm(&["a", ":!a"], "", false, false, true, false)
		.await
		.unwrap();
	assert!(
		outcome.removed.is_empty(),
		"nothing removed: {:?}",
		outcome.removed
	);
	assert!(git(&["-C", w, "ls-files"]).contains('a'));

	std::fs::remove_dir_all(&work).ok();
}

/// A negative-only `rm ':!keep'` applies the exclusion to an implicit `.` and so still requires `-r`,
/// exactly as `rm .` does (probed vs git 2.50.1).
#[tokio::test]
async fn rm_negative_only_requires_recursive() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-negonly");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a"), b"1\n").unwrap();
	std::fs::write(work.join("keep"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	let result = make_repo(&work)
		.rm(&[":!keep"], "", false, false, false, false)
		.await;
	assert!(matches!(result, Err(WorktreeError::RecursiveRequired(_))));

	std::fs::remove_dir_all(&work).ok();
}

/// The implicit `.` of a negative-only `rm` matches every tracked path under the prefix *before* the
/// exclusions apply, so `-r` is required even when the negatives exclude every file — `rm ':!a'` in a
/// repo tracking only `a` still errors `RecursiveRequired` (probed vs git 2.50.1), and does not
/// silently succeed as "nothing selected" would.
#[tokio::test]
async fn rm_negative_only_excluding_all_still_requires_recursive() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-negonly-all");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a"), b"1\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	// Without -r: the implicit `.` matched `a`, so recursion is required even though `:!a` excludes it.
	let result = make_repo(&work)
		.rm(&[":!a"], "", false, false, false, false)
		.await;
	assert!(matches!(result, Err(WorktreeError::RecursiveRequired(_))));

	// With -r: a is excluded, so nothing is removed — a success, not a "did not match" error.
	let outcome = make_repo(&work)
		.rm(&[":!a"], "", false, false, true, false)
		.await
		.unwrap();
	assert!(
		outcome.removed.is_empty(),
		"nothing removed: {:?}",
		outcome.removed
	);
	assert!(git(&["-C", w, "ls-files"]).contains('a'));

	std::fs::remove_dir_all(&work).ok();
}

/// A top-magic pathspec whose non-empty path resolves to the root (`:/.`, `:(top).`) matches NOTHING —
/// git reports it unmatched — so `rm ':/.'` must error, not remove the whole tree (a destructive hazard).
/// A bare `:/` still matches everything. Probed vs git 2.50.1.
#[tokio::test]
async fn rm_top_magic_dot_matches_nothing() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-top-dot");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("sub")).unwrap();
	std::fs::write(work.join("a"), b"1\n").unwrap();
	std::fs::write(work.join("sub/b"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	for spec in [":/.", ":(top)."] {
		let result = make_repo(&work)
			.rm(&[spec], "", false, false, true, false)
			.await;
		assert!(
			matches!(result, Err(WorktreeError::PathspecMatch(_))),
			"`rm {spec}` must be a did-not-match, not remove everything"
		);
	}
	// Nothing was removed; the tree is intact.
	assert_eq!(git(&["-C", w, "ls-files"]).lines().count(), 2);
	// A bare `:/` still removes everything.
	let all = make_repo(&work)
		.rm(&[":/"], "", false, false, true, false)
		.await
		.unwrap();
	assert_eq!(all.removed.len(), 2);

	std::fs::remove_dir_all(&work).ok();
}

/// A root-wide pathspec (`.`, `:/`, `:`) expands the whole tree, so `rm` without `-r` is refused exactly
/// as git refuses `rm .` — the `-r` safety guard must not be bypassed just because the normalized path is
/// empty (probed vs git 2.50.1).
#[tokio::test]
async fn rm_root_pathspec_requires_recursive() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-root-r");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("d")).unwrap();
	std::fs::write(work.join("d/f"), b"1\n").unwrap();
	std::fs::write(work.join("top"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	for spec in [".", ":/", ":"] {
		let result = make_repo(&work)
			.rm(&[spec], "", false, false, false, false)
			.await;
		assert!(
			matches!(result, Err(WorktreeError::RecursiveRequired(_))),
			"`rm {spec}` without -r must be refused"
		);
	}
	// With -r the whole tree is removed, matching git.
	let outcome = make_repo(&work)
		.rm(&["."], "", false, false, true, false)
		.await
		.unwrap();
	let mut removed = outcome.removed.clone();
	removed.sort();
	assert_eq!(removed, vec!["d/f".to_owned(), "top".to_owned()]);

	std::fs::remove_dir_all(&work).ok();
}

/// A wildcard pathspec whose literal spelling names a directory expands it like a leading directory —
/// `rm 'a?'` selects the contents of the literally-named `a?/` (needing `-r`) but NOT `ax/` (which only
/// the glob pass could reach, and it cannot full-match a longer path). Probed vs git 2.50.1.
#[tokio::test]
async fn rm_wildcard_spelling_expands_literal_directory() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-wild-litdir");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("a?")).unwrap();
	std::fs::create_dir_all(work.join("ax")).unwrap();
	std::fs::write(work.join("a?/f"), b"1\n").unwrap();
	std::fs::write(work.join("ax/f"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	// Without -r the leading-directory expansion is refused, exactly as git does.
	let no_r = make_repo(&work)
		.rm(&["a?"], "", false, false, false, false)
		.await;
	assert!(matches!(no_r, Err(WorktreeError::RecursiveRequired(_))));

	// With -r it removes only the literally-named `a?/` contents, never `ax/f`.
	let outcome = make_repo(&work)
		.rm(&["a?"], "", false, false, true, false)
		.await
		.unwrap();
	assert_eq!(outcome.removed, vec!["a?/f".to_owned()]);

	std::fs::remove_dir_all(&work).ok();
}

/// `-r` is waived when a wildcard pathspec ALSO matches a plain file, not only a leading-directory
/// expansion: `rm 'a?'` with tracked `a?/f` (dir) plus `aa` and `ax` (files the glob matches) removes
/// all three without `-r`, unlike a pathspec whose only match is the directory (probed vs git 2.50.1).
#[tokio::test]
async fn rm_wildcard_with_a_file_match_waives_recursive() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-wild-file-waive");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::create_dir_all(work.join("a?")).unwrap();
	std::fs::write(work.join("a?/f"), b"1\n").unwrap();
	std::fs::write(work.join("aa"), b"2\n").unwrap();
	std::fs::write(work.join("ax"), b"3\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	// No -r: succeeds because `aa`/`ax` are plain-file matches; removes all three.
	let outcome = make_repo(&work)
		.rm(&["a?"], "", false, false, false, false)
		.await
		.unwrap();
	let mut removed = outcome.removed.clone();
	removed.sort();
	assert_eq!(
		removed,
		vec!["a?/f".to_owned(), "aa".to_owned(), "ax".to_owned()]
	);

	std::fs::remove_dir_all(&work).ok();
}

/// A pathspec ending in a dangling backslash matches only its literal spelling — git's wildmatch fails
/// on the trailing `\`, so `rm '?\'` removes only the file literally named `?\`, never every two-char
/// path ending in `\` (which the old glob-as-literal behaviour would destructively over-select). Probed
/// vs git 2.50.1.
#[tokio::test]
async fn rm_dangling_backslash_is_literal_only() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-dangle-bs");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("?\\"), b"1\n").unwrap();
	std::fs::write(work.join("x\\"), b"2\n").unwrap();
	git(&["-C", w, "add", "-A"]);
	commit(w, "base");

	let outcome = make_repo(&work)
		.rm(&["?\\"], "", false, false, false, false)
		.await
		.unwrap();
	assert_eq!(outcome.removed, vec!["?\\".to_owned()]);

	std::fs::remove_dir_all(&work).ok();
}

/// A negative-only `rm` whose implicit `.` matches nothing — an empty repository — is git's "did not
/// match any files" error, not a silent success (probed vs git 2.50.1: `rm :!nope` → exit 128).
#[tokio::test]
async fn rm_negative_only_no_candidate_is_pathspec_match() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("rm-negonly-nomatch");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"--allow-empty",
		"-m",
		"empty",
	]);

	let result = make_repo(&work)
		.rm(&[":!nope"], "", false, false, true, false)
		.await;
	assert!(matches!(result, Err(WorktreeError::PathspecMatch(_))));

	std::fs::remove_dir_all(&work).ok();
}

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
		let probe = unique_tmp("probe-rm");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
