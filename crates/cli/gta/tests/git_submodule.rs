//! Faithful gitlink / submodule (tree mode `160000`) handling, cross-checked against real git 2.55.
//! A gitlink entry's object id is a *commit* in the submodule's own repository, not a blob here — gitana
//! must record and report it as such (never map it to a `100644` blob).

use std::path::PathBuf;
use std::process::Command;

/// Staging a gitlink (`160000`) and committing must record a real gitlink tree entry — not a `100644`
/// blob pointing at a commit (which `git fsck` rejects). The written tree must be byte-identical to git's.
#[test]
fn commit_preserves_a_gitlink_entry_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// Any real commit id serves as the recorded submodule commit — git commits a gitlink without the
	// submodule being present.
	let stage_gitlink = |w: &str| -> String {
		std::fs::write(format!("{w}/a.txt"), b"a\n").unwrap();
		git(w, &["add", "a.txt"]);
		commit(w, "base");
		let sub_commit = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
		git(
			w,
			&[
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("160000,{sub_commit},sub"),
			],
		);
		sub_commit
	};

	let a = unique_tmp("gta-sub-commit-gta");
	let b = unique_tmp("gta-sub-commit-git");
	let (wa, wb) = (a.to_str().unwrap(), b.to_str().unwrap());
	git(
		wa,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	git(wa, &["config", "user.name", "T"]);
	git(wa, &["config", "user.email", "t@e"]);
	git(
		wb,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	git(wb, &["config", "user.name", "T"]);
	git(wb, &["config", "user.email", "t@e"]);
	let sub_commit = stage_gitlink(wa);
	stage_gitlink(wb);

	gta(wa, &["commit", "-m", "add submodule"], b"");
	commit(wb, "add submodule");

	// gta's committed tree entry for `sub` must be a gitlink, byte-identical to git's.
	let gta_entry = git(wa, &["ls-tree", "HEAD", "sub"]);
	assert_eq!(
		gta_entry.trim(),
		format!("160000 commit {sub_commit}\tsub"),
		"gta must record a `160000 commit` gitlink, not a blob"
	);
	assert_eq!(
		gta_entry,
		git(wb, &["ls-tree", "HEAD", "sub"]),
		"gta's gitlink tree entry must match git's"
	);
	// git must accept the whole object graph gta wrote.
	assert!(
		Command::new("git")
			.args(["-C", wa, "fsck", "--strict"])
			.output()
			.expect("run git fsck")
			.status
			.success(),
		"git fsck must accept gta's gitlink commit"
	);

	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

fn commit(dir: &str, msg: &str) {
	git(
		dir,
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
}
fn gta(dir: &str, args: &[&str], stdin: &[u8]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.write_stdin(stdin.to_vec())
		.output()
		.expect("run gta");
	assert!(
		out.status.success(),
		"gta {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("gta stdout utf8")
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
		let probe = unique_tmp("gta-probe");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
