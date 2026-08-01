//! `gta prune` / `gta gc` end-to-end, cross-checked with stock git: an unreachable loose object
//! is removed, while objects reachable from refs, HEAD, the index, or the reflog survive; prune
//! refuses while an operation is in progress; and `gc` repacks then prunes. Stock `git fsck`
//! validates the repo after every prune.

use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn prune_removes_a_dangling_object_and_git_stays_clean() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-prune");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	write_add_commit(w, &work, "a.txt", "alpha", "one");
	write_add_commit(w, &work, "b.txt", "beta", "two");
	let before = all_object_ids(w);

	// A loose blob referenced by nothing (not a ref, HEAD, index, or reflog).
	std::fs::write(work.join("dangling"), "dangling content unique 42\n").unwrap();
	let dangling = git(w, &["hash-object", "-w", "dangling"]).trim().to_owned();
	assert!(
		git_ok(w, &["cat-file", "-e", &dangling]),
		"blob exists pre-prune"
	);

	let out = gta(w, &["prune"], b"");
	assert!(out.contains("Pruned 1"), "prune report: {out}");

	// The dangling blob is gone; every committed object remains; git is happy.
	assert!(
		!git_ok(w, &["cat-file", "-e", &dangling]),
		"dangling pruned"
	);
	for id in &before {
		assert!(git_ok(w, &["cat-file", "-e", id]), "kept {id}");
	}
	assert!(git_ok(w, &["fsck", "--full", "--strict"]), "git fsck");

	// Idempotent: nothing left to prune.
	assert!(gta(w, &["prune"], b"").contains("Pruned 0"));
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn prune_keeps_a_staged_but_uncommitted_blob() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-prune-index");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	write_add_commit(w, &work, "a.txt", "alpha", "one");

	// Stage a new file but do not commit: its blob is loose and referenced only by the index.
	std::fs::write(work.join("staged.txt"), "staged only\n").unwrap();
	git(w, &["add", "staged.txt"]);
	let staged = git(w, &["rev-parse", ":staged.txt"]).trim().to_owned();

	gta(w, &["prune"], b"");
	assert!(
		git_ok(w, &["cat-file", "-e", &staged]),
		"staged blob {staged} must survive prune (index is a root)"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn prune_keeps_a_reflog_reachable_commit() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-prune-reflog");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	write_add_commit(w, &work, "a.txt", "alpha", "one");
	write_add_commit(w, &work, "b.txt", "beta", "two");
	let orphan = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	// Move the branch back: the old tip is now unreachable from refs/HEAD but still in the reflog.
	git_id(w, &["reset", "--hard", "HEAD~1"]);
	assert!(
		git_ok(w, &["cat-file", "-e", &orphan]),
		"orphan exists after reset"
	);

	gta(w, &["prune"], b"");
	assert!(
		git_ok(w, &["cat-file", "-e", &orphan]),
		"reflog-reachable commit {orphan} must be kept"
	);
	assert!(git_ok(w, &["fsck", "--full", "--strict"]));
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn prune_refuses_while_an_operation_is_in_progress() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-prune-refuse");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	write_add_commit(w, &work, "f.txt", "base", "root");
	git_id(w, &["branch", "other"]);
	write_add_commit(w, &work, "f.txt", "ours", "ours");
	git_id(w, &["checkout", "other"]);
	write_add_commit(w, &work, "f.txt", "theirs", "theirs");
	git_id(w, &["checkout", "main"]);

	// A conflicting merge leaves MERGE_HEAD in place. The identity flags matter: git refuses to *start*
	// a merge with no committer identity (a bare runner has none), erroring out before it can conflict —
	// so pass one, or MERGE_HEAD is never written and there is no in-progress state for prune to refuse.
	assert!(
		!git_ok(
			w,
			&[
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"merge",
				"other"
			]
		),
		"merge should conflict"
	);

	std::fs::write(work.join("dangling"), "unique dangling 99\n").unwrap();
	let dangling = git(w, &["hash-object", "-w", "dangling"]).trim().to_owned();

	assert!(
		!gta_ok(w, &["prune"]),
		"prune must refuse while a merge is in progress"
	);
	assert!(
		git_ok(w, &["cat-file", "-e", &dangling]),
		"a refused prune deletes nothing"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn prune_refuses_during_a_stock_git_rebase() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-prune-rebase");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	// Set identity in config so a plain `git rebase` can run to the conflict point.
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	write_add_commit(w, &work, "f.txt", "base", "base");
	git_id(w, &["branch", "feature"]);
	write_add_commit(w, &work, "f.txt", "main", "main");
	git_id(w, &["checkout", "feature"]);
	write_add_commit(w, &work, "f.txt", "feature", "feature");

	// A conflicting rebase leaves `.git/rebase-merge/` in place (stock git's own state, not
	// gitana's flat REBASE_* files).
	assert!(!git_ok(w, &["rebase", "main"]), "rebase should conflict");

	std::fs::write(work.join("dangling"), "unique dangling rebase 5\n").unwrap();
	let dangling = git(w, &["hash-object", "-w", "dangling"]).trim().to_owned();
	assert!(
		!gta_ok(w, &["prune"]),
		"prune must refuse during a stock-git rebase"
	);
	assert!(
		git_ok(w, &["cat-file", "-e", &dangling]),
		"a refused prune deletes nothing"
	);
	git_id(w, &["rebase", "--abort"]);
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn prune_refuses_when_linked_worktrees_exist() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-prune-linked");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	write_add_commit(w, &work, "a.txt", "alpha", "one");

	// A linked worktree has its own HEAD/index/reflog that this single-worktree prune cannot see.
	let linked = work.join("linked");
	git_id(
		w,
		&[
			"worktree",
			"add",
			"--detach",
			linked.to_str().unwrap(),
			"HEAD",
		],
	);

	std::fs::write(work.join("dangling"), "linked unique 3\n").unwrap();
	let dangling = git(w, &["hash-object", "-w", "dangling"]).trim().to_owned();
	assert!(
		!gta_ok(w, &["prune"]),
		"prune must refuse while linked worktrees exist"
	);
	assert!(
		git_ok(w, &["cat-file", "-e", &dangling]),
		"a refused prune deletes nothing"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn gc_repacks_and_prunes() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-gc");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	write_add_commit(w, &work, "a.txt", "alpha", "one");
	write_add_commit(w, &work, "b.txt", "beta", "two");
	std::fs::write(work.join("dangling"), "unique dangling gc 7\n").unwrap();
	let dangling = git(w, &["hash-object", "-w", "dangling"]).trim().to_owned();
	let committed = all_object_ids(w);

	gta(w, &["gc"], b"");

	// One pack, dangling object gone, committed history intact, git clean.
	let packs = pack_files(&work);
	assert_eq!(packs.iter().filter(|p| p.ends_with(".pack")).count(), 1);
	assert!(
		!git_ok(w, &["cat-file", "-e", &dangling]),
		"gc prunes dangling"
	);
	assert!(!has_loose_objects(&work), "gc packs then prunes all loose");
	for id in &committed {
		if id != &dangling {
			assert!(git_ok(w, &["cat-file", "-e", id]), "kept {id}");
		}
	}
	assert!(git_ok(w, &["fsck", "--full", "--strict"]));

	// gc also wrote a multi-pack-index and a reachability bitmap that stock git reads and trusts.
	let pack = pack_files(&work);
	assert!(
		pack.iter().any(|p| p == "multi-pack-index"),
		"gc wrote a multi-pack-index: {pack:?}"
	);
	assert!(
		pack
			.iter()
			.any(|p| p.starts_with("multi-pack-index-") && p.ends_with(".bitmap")),
		"gc wrote a reachability bitmap: {pack:?}"
	);
	assert!(
		git_ok(w, &["multi-pack-index", "verify"]),
		"git multi-pack-index verify accepts gc's output"
	);
	assert!(
		git_ok(w, &["rev-list", "--test-bitmap", "HEAD"]),
		"git rev-list --test-bitmap accepts gc's bitmap"
	);

	// Idempotent: already packed, nothing to prune.
	let out = gta(w, &["gc"], b"");
	assert!(out.contains("Nothing to repack"), "second gc: {out}");
	assert_eq!(
		pack_files(&work)
			.iter()
			.filter(|p| p.ends_with(".pack"))
			.count(),
		1
	);
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn gc_bitmaps_annotated_tag_commits() {
	use gitana_object::{ObjectId, Sha256, decode_midx_bitmap, decode_multi_pack_index};

	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-gc-tag");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	write_add_commit(w, &work, "a.txt", "alpha", "one");
	write_add_commit(w, &work, "b.txt", "beta", "two");

	// Annotate the FIRST commit (not HEAD), so only tag-peeling can reach it as a bitmap tip.
	let mut tag_args = vec!["-c", "user.name=T", "-c", "user.email=t@example.com"];
	tag_args.extend_from_slice(&["tag", "-a", "-m", "release", "v1", "HEAD~1"]);
	git(w, &tag_args);

	gta(w, &["gc"], b"");

	// Decode gc's MIDX + bitmap with our own reader; both the HEAD commit and the tagged commit
	// (reachable only by peeling the annotated tag) must be bitmapped.
	let pack_dir = work.join(".git/objects/pack");
	let midx =
		decode_multi_pack_index::<Sha256>(&std::fs::read(pack_dir.join("multi-pack-index")).unwrap())
			.unwrap();
	let bitmap_name = pack_files(&work)
		.into_iter()
		.find(|p| p.starts_with("multi-pack-index-") && p.ends_with(".bitmap"))
		.expect("gc wrote a bitmap");
	let index =
		decode_midx_bitmap::<Sha256>(&std::fs::read(pack_dir.join(bitmap_name)).unwrap()).unwrap();

	let head = ObjectId::<Sha256>::from_hex(git(w, &["rev-parse", "HEAD"]).trim()).unwrap();
	let tagged = ObjectId::<Sha256>::from_hex(git(w, &["rev-parse", "HEAD~1"]).trim()).unwrap();
	assert!(
		index.reachable_from(&head, &midx).is_some(),
		"HEAD commit is bitmapped"
	);
	assert!(
		index.reachable_from(&tagged, &midx).is_some(),
		"annotated-tag commit is bitmapped via peeling"
	);

	std::fs::remove_dir_all(&work).ok();
}

// --- helpers -------------------------------------------------------------------------------

fn has_loose_objects(work: &Path) -> bool {
	let objects = work.join(".git/objects");
	let Ok(entries) = std::fs::read_dir(&objects) else {
		return false;
	};
	entries.filter_map(|e| e.ok()).any(|entry| {
		let name = entry.file_name();
		let name = name.to_string_lossy();
		name.len() == 2
			&& name.bytes().all(|b| b.is_ascii_hexdigit())
			&& std::fs::read_dir(entry.path())
				.map(|mut d| d.next().is_some())
				.unwrap_or(false)
	})
}

fn pack_files(work: &Path) -> Vec<String> {
	let pack = work.join(".git/objects/pack");
	let Ok(entries) = std::fs::read_dir(&pack) else {
		return Vec::new();
	};
	entries
		.filter_map(|e| e.ok())
		.map(|e| e.file_name().to_string_lossy().into_owned())
		.collect()
}

fn all_object_ids(w: &str) -> Vec<String> {
	let mut ids: Vec<String> = git(
		w,
		&[
			"cat-file",
			"--batch-check=%(objectname)",
			"--batch-all-objects",
			"--unordered",
		],
	)
	.lines()
	.map(str::to_owned)
	.collect();
	ids.sort();
	ids
}

fn write_add_commit(w: &str, work: &Path, file: &str, content: &str, msg: &str) {
	std::fs::write(work.join(file), format!("{content}\n")).unwrap();
	git(w, &["add", "."]);
	git_id(w, &["commit", "-q", "-m", msg]);
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

fn gta_ok(dir: &str, args: &[&str]) -> bool {
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.output()
		.expect("run gta")
		.status
		.success()
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

fn git_id(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir, "-c", "user.name=T", "-c", "user.email=t@e"];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn git_ok(dir: &str, args: &[&str]) -> bool {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	Command::new("git")
		.args(&full)
		.output()
		.expect("run git")
		.status
		.success()
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-prune");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
