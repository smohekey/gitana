//! `gta repack` writes a git-compatible multi-pack-index: after a size-bounded repack splits a
//! repo across packs, stock `git multi-pack-index verify` accepts our `multi-pack-index`, `git fsck`
//! stays clean, and the full object set is unchanged.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn git_verifies_the_multi_pack_index_gta_writes() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-midx");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);
	git(w, &["config", "pack.packSizeLimit", "1m"]);

	// ~2.4 MiB of incompressible files → several packs under the 1 MiB limit.
	for i in 0..8u64 {
		std::fs::write(
			work.join(format!("big{i}.bin")),
			incompressible(i, 300 * 1024),
		)
		.unwrap();
	}
	git(w, &["add", "."]);
	git_id(w, &["commit", "-q", "-m", "big"]);
	let before = all_object_ids(w);

	gta(w, &["repack"], b"");

	// A multi-pack repack writes a multi-pack-index over several packs.
	let pack_dir = work.join(".git/objects/pack");
	assert!(
		pack_dir.join("multi-pack-index").exists(),
		"gta repack wrote a multi-pack-index"
	);
	let packs = std::fs::read_dir(&pack_dir)
		.unwrap()
		.filter_map(|e| e.ok())
		.filter(|e| e.file_name().to_string_lossy().ends_with(".pack"))
		.count();
	assert!(packs > 1, "expected multiple packs, got {packs}");

	// Stock git accepts our MIDX and the repo, and sees the same object set.
	assert!(
		git_ok(w, &["multi-pack-index", "verify"]),
		"git multi-pack-index verify must accept our MIDX"
	);
	assert!(git_ok(w, &["fsck", "--full", "--strict"]), "git fsck");
	assert_eq!(all_object_ids(w), before, "every object preserved");

	std::fs::remove_dir_all(&work).ok();
}

// --- helpers -------------------------------------------------------------------------------

fn incompressible(seed: u64, len: usize) -> Vec<u8> {
	let mut x = seed.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
	let mut out = Vec::with_capacity(len);
	for _ in 0..len {
		x ^= x << 13;
		x ^= x >> 7;
		x ^= x << 17;
		out.push((x & 0xff) as u8);
	}
	out
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
		let probe = unique_tmp("probe-midx");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
