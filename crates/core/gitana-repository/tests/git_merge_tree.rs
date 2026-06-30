//! `Repository::merge_trees` cross-checked against `git merge-tree --write-tree`: a clean,
//! well-separated three-way merge must produce the byte-identical tree git does, and a divergent
//! edit must be reported as a conflict (which git also rejects).

use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_file_store_local::LocalFileStore;
use gitana_object::{ObjectId, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;

#[tokio::test]
async fn clean_merge_matches_git_merge_tree() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("merge-tree-clean");
	let w = work.to_str().unwrap();

	// base, then two branches that touch well-separated lines of a shared file and each add a file.
	git(w, &["init", "-q", "--object-format=sha256", "."]);
	write(&work, "a.txt", "base\n");
	write(&work, "shared.txt", "1\n2\n3\n4\n5\n");
	let base = commit_all(w, "base");
	checkout_new(w, "ours", &base);
	write(&work, "shared.txt", "OURS\n2\n3\n4\n5\n");
	write(&work, "ours.txt", "o\n");
	let ours = commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	write(&work, "shared.txt", "1\n2\n3\n4\nTHEIRS\n");
	write(&work, "theirs.txt", "t\n");
	let theirs = commit_all(w, "theirs");

	let merged = repo(&work)
		.merge_trees(tree_of(w, &base), tree_of(w, &ours), tree_of(w, &theirs))
		.await
		.unwrap();
	assert!(
		merged.conflicts.is_empty(),
		"unexpected: {:?}",
		merged.conflicts
	);

	// git performs the same three-way merge (computing the same merge base, `base`) and writes the
	// tree; the first output line is its oid.
	let git_tree = git(w, &["merge-tree", "--write-tree", &ours, &theirs]);
	assert_eq!(
		merged.tree.to_hex(),
		git_tree.lines().next().unwrap().trim()
	);

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn divergent_edit_is_a_conflict() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("merge-tree-conflict");
	let w = work.to_str().unwrap();

	git(w, &["init", "-q", "--object-format=sha256", "."]);
	write(&work, "shared.txt", "1\n2\n3\n");
	let base = commit_all(w, "base");
	checkout_new(w, "ours", &base);
	write(&work, "shared.txt", "1\nOURS\n3\n");
	let ours = commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	write(&work, "shared.txt", "1\nTHEIRS\n3\n");
	let theirs = commit_all(w, "theirs");

	let merged = repo(&work)
		.merge_trees(tree_of(w, &base), tree_of(w, &ours), tree_of(w, &theirs))
		.await
		.unwrap();
	assert_eq!(merged.conflicts, ["shared.txt"]);

	// git also reports a conflict: `merge-tree --write-tree` exits non-zero.
	let ok = Command::new("git")
		.args(["-C", w, "merge-tree", "--write-tree", &ours, &theirs])
		.output()
		.unwrap()
		.status
		.success();
	assert!(!ok, "git should report the same conflict");

	std::fs::remove_dir_all(&work).ok();
}

#[tokio::test]
async fn divergent_binary_is_a_conflict_keeping_ours() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("merge-tree-binary");
	let w = work.to_str().unwrap();

	// `f.bin` has an embedded NUL, so it is binary and cannot be line-merged.
	git(w, &["init", "-q", "--object-format=sha256", "."]);
	write_bytes(&work, "f.bin", b"A\x00base\n");
	let base = commit_all(w, "base");
	checkout_new(w, "ours", &base);
	write_bytes(&work, "f.bin", b"A\x00OURS\n");
	let ours = commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	write_bytes(&work, "f.bin", b"A\x00THEIRS\n");
	let theirs = commit_all(w, "theirs");

	let r = repo(&work);
	let merged = r
		.merge_trees(tree_of(w, &base), tree_of(w, &ours), tree_of(w, &theirs))
		.await
		.unwrap();
	assert_eq!(merged.conflicts, ["f.bin"]);

	// gta keeps ours's binary payload uncorrupted (no conflict markers spliced in).
	let (_, _, id) = r
		.read_tree(merged.tree)
		.await
		.unwrap()
		.into_iter()
		.find(|(path, _, _)| path == "f.bin")
		.unwrap();
	assert_eq!(r.read_blob(id).await.unwrap(), b"A\x00OURS\n");

	// git also reports the conflict and writes the same tree (it keeps ours too).
	let out = Command::new("git")
		.args(["-C", w, "merge-tree", "--write-tree", &ours, &theirs])
		.output()
		.unwrap();
	assert!(!out.status.success(), "git should report a binary conflict");
	let git_tree = String::from_utf8(out.stdout).unwrap();
	assert_eq!(
		merged.tree.to_hex(),
		git_tree.lines().next().unwrap().trim()
	);

	std::fs::remove_dir_all(&work).ok();
}

#[cfg(unix)]
#[tokio::test]
async fn binary_mode_and_content_change_on_opposite_sides_matches_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("merge-tree-mode-bin");
	let w = work.to_str().unwrap();

	// base: a regular binary file. ours: flip only the mode to executable. theirs: change only the
	// (binary) content. The changes don't overlap, so git merges them cleanly.
	git(w, &["init", "-q", "--object-format=sha256", "."]);
	write_bytes(&work, "f.bin", b"A\x00base\n");
	let base = commit_all(w, "base");
	checkout_new(w, "ours", &base);
	make_executable(&work, "f.bin");
	let ours = commit_all(w, "ours");
	checkout_new(w, "theirs", &base);
	write_bytes(&work, "f.bin", b"A\x00THEIRS\n");
	let theirs = commit_all(w, "theirs");

	let r = repo(&work);
	let merged = r
		.merge_trees(tree_of(w, &base), tree_of(w, &ours), tree_of(w, &theirs))
		.await
		.unwrap();
	assert!(
		merged.conflicts.is_empty(),
		"unexpected: {:?}",
		merged.conflicts
	);
	let (_, mode, id) = r
		.read_tree(merged.tree)
		.await
		.unwrap()
		.into_iter()
		.find(|(path, _, _)| path == "f.bin")
		.unwrap();
	assert_eq!(mode, "100755");
	assert_eq!(r.read_blob(id).await.unwrap(), b"A\x00THEIRS\n");

	// git merges to the same tree (exit 0).
	let git_tree = git(w, &["merge-tree", "--write-tree", &ours, &theirs]);
	assert_eq!(
		merged.tree.to_hex(),
		git_tree.lines().next().unwrap().trim()
	);

	std::fs::remove_dir_all(&work).ok();
}

#[cfg(unix)]
fn make_executable(work: &Path, name: &str) {
	use std::os::unix::fs::PermissionsExt;
	let path = work.join(name);
	let mut perms = std::fs::metadata(&path).unwrap().permissions();
	perms.set_mode(0o755);
	std::fs::set_permissions(&path, perms).unwrap();
}

fn repo(work: &Path) -> Repository<LocalFileStore, Sha256> {
	Repository::new(ObjectStore::new(LocalFileStore::new(work.join(".git"))))
}

fn tree_of(w: &str, commit: &str) -> ObjectId<Sha256> {
	ObjectId::from_hex(git(w, &["rev-parse", &format!("{commit}^{{tree}}")]).trim()).unwrap()
}

fn write(work: &Path, name: &str, content: &str) {
	std::fs::write(work.join(name), content).unwrap();
}

fn write_bytes(work: &Path, name: &str, content: &[u8]) {
	std::fs::write(work.join(name), content).unwrap();
}

fn commit_all(w: &str, msg: &str) -> String {
	git(w, &["add", "."]);
	git(
		w,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			msg,
		],
	);
	git(w, &["rev-parse", "HEAD"]).trim().to_owned()
}

fn checkout_new(w: &str, branch: &str, start: &str) {
	git(w, &["checkout", "-q", "-b", branch, start]);
}

fn git(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
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
	let dir = std::env::temp_dir().join(format!("gitana-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-merge-tree");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
