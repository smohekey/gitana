#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

use gitana_file_store_local::LocalFileStore;
use gitana_object::Sha256;
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{IndexEntry, Stat, WorkTree, WorktreeError};

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn make_repo(work: &std::path::Path) -> WorkTree<LocalFileStore, Sha256> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, work, git_dir)
}

#[tokio::test]
async fn mv_rejects_unsafe_index_source() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("mv-unsafe");
	let w = work.to_str().unwrap();
	git(&["init", "--object-format=sha256", "-q", w]);
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git(&["-C", w, "add", "."]);
	commit(w, "one");
	let wt = make_repo(&work);

	// Inject a hostile entry naming a path inside `.git`, as a corrupt index might carry. Without
	// source validation, `mv` would treat it as a tracked file and rename the real `.git/config`.
	let mut index = wt.load_index().unwrap();
	let blob = wt.repository().write_blob(b"PWN\n").await.unwrap();
	index.upsert(IndexEntry {
		stat: Stat::default(),
		mode: 0o100644,
		oid: blob,
		stage: 0,
		assume_valid: false,
		path: ".git/config".to_owned(),
	});
	wt.save_index(&index).unwrap();
	let config_before = std::fs::read(work.join(".git/config")).unwrap();

	assert!(matches!(
		wt.mv(&[".git/config"], "stolen", "", false, false).await,
		Err(WorktreeError::UnsafePath(_))
	));
	// The real config was not moved out, and no `stolen` file was created.
	assert_eq!(
		std::fs::read(work.join(".git/config")).unwrap(),
		config_before
	);
	assert!(!work.join("stolen").exists());

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
		let probe = unique_tmp("probe-mv");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
