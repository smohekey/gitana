use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::{HashAlgorithm, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{Index, WorkTree};

#[test]
fn round_trips_a_real_git_index() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("index");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);

	std::fs::write(work.join("README.md"), b"readme\n").unwrap();
	std::fs::create_dir_all(work.join("src")).unwrap();
	std::fs::write(work.join("src/lib.rs"), b"lib\n").unwrap();
	std::fs::write(work.join("src/main.rs"), b"main\n").unwrap();
	// Have git write a version-4 index so we exercise v4 parsing.
	git(&["-C", w, "-c", "index.version=4", "add", "."]);

	let git_listing = ls_files(w);

	// Our parser reads git's v4 index and agrees on mode/oid/path.
	let index =
		Index::<Sha256>::parse(&std::fs::read(work.join(".git/index")).unwrap()).expect("parse");
	let ours: Vec<String> = index
		.entries
		.iter()
		.map(|e| format!("{:o} {} {}\t{}", e.mode, e.oid.to_hex(), e.stage, e.path))
		.collect();
	assert_eq!(ours, git_listing, "our parse must match git ls-files");

	// git reads our re-written v4 index identically.
	std::fs::write(work.join(".git/index"), index.write_v4()).unwrap();
	assert_eq!(ls_files(w), git_listing, "git must read our v4 index");
	// And git is otherwise happy with the index we wrote.
	git(&["-C", w, "status", "--porcelain"]);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn round_trips_a_skip_worktree_entry() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("index-skipwt");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"a\n").unwrap();
	std::fs::write(work.join("b.txt"), b"b\n").unwrap();
	git(&["-C", w, "-c", "index.version=4", "add", "a.txt", "b.txt"]);
	git(&["-C", w, "update-index", "--skip-worktree", "b.txt"]);

	// We parse the skip-worktree flag...
	let index =
		Index::<Sha256>::parse(&std::fs::read(work.join(".git/index")).unwrap()).expect("parse");
	let a = gitana_path::GitPath::from_utf8("a.txt").unwrap();
	let b = gitana_path::GitPath::from_utf8("b.txt").unwrap();
	assert!(
		index.entry(&b).unwrap().skip_worktree,
		"b.txt should be parsed as skip-worktree"
	);
	assert!(
		!index.entry(&a).unwrap().skip_worktree,
		"a.txt should not be skip-worktree"
	);

	// ...and preserve it on write: git still reports b.txt skip-worktree (an `S` in ls-files -t / a `s` here).
	std::fs::write(work.join(".git/index"), index.write_v4()).unwrap();
	let flags = git(&["-C", w, "ls-files", "-v"]);
	assert!(
		flags
			.lines()
			.any(|l| l.starts_with("S ") && l.ends_with("b.txt")),
		"git must still see b.txt as skip-worktree after our rewrite, got:\n{flags}"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// Under `index.skipHash`, which `feature.manyFiles` also turns on, git writes all zeros where the
/// index's trailing hash goes. We read such an index as git does, and git reads our rewrite of it.
///
/// Its git is [`isolated_git`]: an explicit `index.skipHash=false` in the machine's config would beat the
/// `feature.manyFiles` default this relies on. And it needs git 2.40 or later, where `index.skipHash`
/// arrived (an older git ignores it and writes the hash). That's checked by version rather than by asking
/// git to skip the hash, so a git that ought to skip it and doesn't is a failure, not a skip.
#[test]
fn reads_an_index_git_wrote_without_its_trailing_hash() {
	let version = git_version();
	if version < (2, 40) {
		eprintln!(
			"skipping: git {}.{} predates index.skipHash (2.40)",
			version.0, version.1
		);
		return;
	}
	reads_an_index_without_its_trailing_hash::<Sha1>("sha1");
	if git_supports_sha256() {
		reads_an_index_without_its_trailing_hash::<Sha256>("sha256");
	} else {
		eprintln!("skipping sha256: git without --object-format=sha256");
	}
}

fn reads_an_index_without_its_trailing_hash<H: HashAlgorithm>(format: &str) {
	for setting in ["index.skipHash=true", "feature.manyFiles=true"] {
		let work = unique_tmp("index-skip-hash");
		let w = work.to_str().unwrap();
		let git_listing = || -> Vec<String> {
			isolated_git(&["-C", w, "ls-files", "--stage"])
				.lines()
				.map(str::to_owned)
				.collect()
		};
		isolated_git(&["init", &format!("--object-format={format}"), "-q", w]);
		std::fs::write(work.join("README.md"), b"readme\n").unwrap();
		std::fs::create_dir_all(work.join("src")).unwrap();
		std::fs::write(work.join("src/lib.rs"), b"lib\n").unwrap();
		std::fs::write(work.join("untracked.txt"), b"u\n").unwrap();
		isolated_git(&["-C", w, "-c", setting, "add", "README.md", "src"]);
		// With `feature.manyFiles`, status also records its untracked cache as an index extension.
		isolated_git(&["-C", w, "-c", setting, "status", "--porcelain"]);
		let bytes = std::fs::read(work.join(".git/index")).unwrap();
		assert!(
			bytes[bytes.len() - H::RAW_LEN..]
				.iter()
				.all(|&byte| byte == 0),
			"{format} {setting}: git skipped the hash"
		);

		let listing = git_listing();
		let index = Index::<H>::parse(&bytes)
			.unwrap_or_else(|error| panic!("{format} {setting}: parse: {error}"));
		let ours: Vec<String> = index
			.entries
			.iter()
			.map(|e| format!("{:o} {} {}\t{}", e.mode, e.oid.to_hex(), e.stage, e.path))
			.collect();
		assert_eq!(
			ours, listing,
			"{format} {setting}: our parse must match git ls-files"
		);

		std::fs::write(work.join(".git/index"), index.write_v4()).unwrap();
		assert_eq!(
			git_listing(),
			listing,
			"{format} {setting}: git must read our rewrite"
		);
		std::fs::remove_dir_all(&work).ok();
	}
}

#[tokio::test]
async fn merges_a_split_index_to_match_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("index-split");
	let git_dir = work.join(".git");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	// A base with a few files (incl. nested), committed and shared.
	std::fs::write(work.join("a.txt"), b"1\n").unwrap();
	std::fs::write(work.join("b.txt"), b"2\n").unwrap();
	std::fs::write(work.join("c.txt"), b"3\n").unwrap();
	std::fs::create_dir_all(work.join("dir")).unwrap();
	std::fs::write(work.join("dir/x.txt"), b"x\n").unwrap();
	git(&["-C", w, "add", "."]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);
	git(&["-C", w, "update-index", "--split-index"]);

	// Exercise every split-index operation: modify (replace), delete, add (top-level + nested).
	std::fs::write(work.join("a.txt"), b"1 changed\n").unwrap();
	git(&["-C", w, "add", "a.txt"]);
	git(&["-C", w, "rm", "-q", "b.txt"]);
	std::fs::write(work.join("d.txt"), b"4\n").unwrap();
	std::fs::write(work.join("dir/y.txt"), b"y\n").unwrap();
	git(&["-C", w, "add", "d.txt", "dir/y.txt"]);

	// Sanity: git is really using a split index (a shared index file exists and index carries a `link` ext).
	assert!(
		std::fs::read_dir(&git_dir)
			.unwrap()
			.flatten()
			.any(|e| e.file_name().to_string_lossy().starts_with("sharedindex.")),
		"expected a shared index file"
	);

	// Our merged index must equal git's effective (merged) view, entry for entry.
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let index = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.load_index()
		.await
		.expect("load split index");
	let ours: Vec<String> = index
		.entries
		.iter()
		.map(|e| format!("{:o} {} {}\t{}", e.mode, e.oid.to_hex(), e.stage, e.path))
		.collect();
	assert_eq!(
		ours,
		ls_files(w),
		"merged split index must match git ls-files --stage"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn rejects_a_substituted_shared_index() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let base = unique_tmp("index-split-bad");
	let work = base.join("r1");
	let git_dir = work.join(".git");
	std::fs::create_dir_all(&work).unwrap();
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"1\n").unwrap();
	git(&["-C", w, "add", "a.txt"]);
	git(&[
		"-C",
		w,
		"-c",
		"user.name=T",
		"-c",
		"user.email=t@e",
		"commit",
		"-q",
		"-m",
		"init",
	]);
	git(&["-C", w, "update-index", "--split-index"]);
	std::fs::write(work.join("b.txt"), b"2\n").unwrap();
	git(&["-C", w, "add", "b.txt"]);

	// A *different* but internally-valid index (from a second repo) — its trailer checksum differs from the
	// link oid, so substituting it must be rejected rather than silently building status from the wrong state.
	let other = base.join("r2");
	std::fs::create_dir_all(&other).unwrap();
	let o = other.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", o]);
	std::fs::write(other.join("z.txt"), b"z\n").unwrap();
	git(&["-C", o, "add", "z.txt"]);
	let other_index = std::fs::read(other.join(".git/index")).unwrap();

	// Overwrite every shared index file with the foreign (valid) index.
	for entry in std::fs::read_dir(&git_dir).unwrap().flatten() {
		if entry
			.file_name()
			.to_string_lossy()
			.starts_with("sharedindex.")
		{
			std::fs::write(entry.path(), &other_index).unwrap();
		}
	}

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	let result = WorkTree::new(repo, CapWorkDir::from_dir(open_dir(&work)), &git_dir)
		.load_index()
		.await;
	assert!(
		result.is_err(),
		"a shared index whose checksum does not match the link oid must be rejected, got {result:?}"
	);
	std::fs::remove_dir_all(&base).ok();
}

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn ls_files(work: &str) -> Vec<String> {
	git(&["-C", work, "ls-files", "--stage"])
		.lines()
		.map(str::to_owned)
		.collect()
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

/// A `git` that reads nothing from the machine, as gitana-linked-worktree's fixtures run it: no global or
/// system config file, no default global ignore or attributes file, and no inherited `GIT_*` variable but
/// `GIT_TRACE*` (an inherited `GIT_DIR` would beat `-C`; `GIT_CONFIG_PARAMETERS` would add config). Only
/// some of this file's tests use it so far; the rest use [`git`].
fn isolated_git(args: &[&str]) -> String {
	let mut command = Command::new("git");
	for (name, _) in std::env::vars_os() {
		let spelled = name.to_string_lossy();
		if spelled.starts_with("GIT_") && !spelled.starts_with("GIT_TRACE") {
			command.env_remove(&name);
		}
	}
	let out = command
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("XDG_CONFIG_HOME", "/dev/null")
		.args(args)
		.output()
		.expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

/// The isolated git's `(major, minor)` version, from `git --version` (`git version 2.50.1 (Apple Git-155)`,
/// `git version 2.45.1.windows.1`).
fn git_version() -> (u32, u32) {
	let reported = isolated_git(&["--version"]);
	let parsed = reported.split_whitespace().nth(2).and_then(|version| {
		let mut parts = version.split('.').map(|part| part.parse::<u32>().ok());
		Some((parts.next()??, parts.next()??))
	});
	parsed.unwrap_or_else(|| panic!("unrecognised `git --version`: {reported:?}"))
}

fn unique_tmp(tag: &str) -> PathBuf {
	// A per-call sequence number keeps every temp dir distinct even for a reused tag, so tests running
	// in parallel threads never race on `remove_dir_all`/`create_dir_all` for the same path (which
	// surfaced as a transient `File exists`). Matches the `git_status`/`git_diff`/`git_submodule` harnesses.
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
	let probe = unique_tmp("probe");
	let ok = Command::new("git")
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}
