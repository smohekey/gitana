//! Differential test: [`gitana_object::decode_multi_pack_index`] reads the `multi-pack-index`
//! stock `git multi-pack-index write` produces for a repo with several packs, and agrees with the
//! packs on every object's pack and byte offset.
//!
//! Requires a `git` on `PATH` supporting SHA-256 repositories (git >= 2.29); otherwise skipped.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use gitana_object::{
	ObjectId, ObjectKind, PackedObject, Sha256, decode_multi_pack_index, encode_pack,
	pack_index_entries,
};
use tempfile::TempDir;

/// `n` distinct blobs (distinct content → distinct ids), self-contained so each pack is valid.
fn blobs(range: std::ops::Range<u64>) -> Vec<PackedObject<Sha256>> {
	range
		.map(|i| {
			let data = format!("multi-pack blob number {i}\n")
				.repeat(20)
				.into_bytes();
			let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &data);
			PackedObject {
				id,
				kind: ObjectKind::Blob,
				data,
			}
		})
		.collect()
}

#[test]
fn we_read_git_multi_pack_index() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}

	let repo = TempDir::new().expect("tempdir");
	let repo_path = repo.path().join("repo");
	run_git(&[
		"init",
		"--object-format=sha256",
		"--bare",
		repo_path.to_str().unwrap(),
	]);

	// Two independent packs, installed into the repo. `id → (idx name, offset)` per our packs; the
	// bytes we send are what git stores, so offsets match.
	let mut expected: HashMap<ObjectId<Sha256>, (String, u64)> = HashMap::new();
	let mut total = 0usize;
	for group in [blobs(0..2), blobs(2..4)] {
		let pack = encode_pack(&group);
		let checksum = &pack[pack.len() - 32..];
		let idx_name = format!("pack-{}.idx", hex(checksum));
		for entry in pack_index_entries::<Sha256>(&pack).expect("scan pack") {
			expected.insert(entry.id, (idx_name.clone(), entry.offset));
			total += 1;
		}
		install_pack(&repo_path, &pack);
	}

	run_git(&[
		"-C",
		repo_path.to_str().unwrap(),
		"multi-pack-index",
		"write",
	]);
	let bytes = std::fs::read(repo_path.join("objects/pack/multi-pack-index"))
		.expect("git wrote a multi-pack-index");

	let midx = decode_multi_pack_index::<Sha256>(&bytes).expect("decode git's MIDX");
	assert_eq!(midx.len(), total);
	assert_eq!(midx.pack_names().len(), 2);
	for (id, (idx_name, offset)) in &expected {
		let (pack_index, got) = midx.lookup(id).expect("id present in MIDX");
		assert_eq!(&midx.pack_names()[pack_index], idx_name, "pack for {id}");
		assert_eq!(got, *offset, "offset for {id}");
	}
}

fn install_pack(repo_path: &std::path::Path, pack: &[u8]) {
	let mut child = Command::new("git")
		.arg("-C")
		.arg(repo_path)
		.args(["index-pack", "--stdin"])
		.stdin(Stdio::piped())
		.stdout(Stdio::null())
		.spawn()
		.expect("spawn git index-pack --stdin");
	child
		.stdin
		.take()
		.expect("stdin")
		.write_all(pack)
		.expect("write pack to git");
	assert!(
		child.wait().expect("wait git").success(),
		"git index-pack --stdin failed"
	);
}

fn hex(bytes: &[u8]) -> String {
	let mut s = String::with_capacity(bytes.len() * 2);
	for b in bytes {
		s.push_str(&format!("{b:02x}"));
	}
	s
}

fn run_git(args: &[&str]) {
	let output = Command::new("git").args(args).output().expect("run git");
	assert!(
		output.status.success(),
		"git {args:?} failed:\n{}",
		String::from_utf8_lossy(&output.stderr)
	);
}

fn git_supports_sha256() -> bool {
	let probe = TempDir::new().expect("tempdir");
	Command::new("git")
		.args(["init", "--object-format=sha256"])
		.arg(probe.path().join("probe"))
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}
