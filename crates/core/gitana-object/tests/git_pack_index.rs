//! Differential test: the `.idx` [`gitana_object::encode_pack_index`] produces for a pack
//! is byte-identical to the one stock `git index-pack --object-format=sha256` writes for
//! the same pack, and [`gitana_object::decode_pack_index`] reads git's `.idx` back.
//!
//! Requires a `git` on `PATH` that supports SHA-256 repositories (git >= 2.29); where it
//! does not, the git-oracle assertion is skipped with a printed note.

use std::process::Command;

use gitana_object::{
	Commit, ObjectId, ObjectKind, PackedObject, Sha256, TreeEntry, decode_pack_index, encode_commit,
	encode_pack, encode_pack_index, encode_tree, pack_index_entries,
};
use tempfile::TempDir;

/// A small object graph with two delta-friendly blobs, a tree, and a commit — enough to
/// exercise the fanout, the id sort, per-object CRC-32s over both a full object and a
/// delta, and multiple offsets.
fn sample_graph() -> Vec<PackedObject<Sha256>> {
	let mut objects = Vec::new();
	let mut put = |kind: ObjectKind, data: Vec<u8>| {
		let id = ObjectId::<Sha256>::compute(kind, &data);
		objects.push(PackedObject { id, kind, data });
		id
	};

	let body = b"line one\nline two\nline three\n".repeat(40);
	let blob1 = put(ObjectKind::Blob, body.clone());
	let mut body2 = body;
	body2.extend_from_slice(b"line four added later\n");
	put(ObjectKind::Blob, body2);

	let tree = put(
		ObjectKind::Tree,
		encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: "file.txt".to_owned(),
			id: blob1,
		}]),
	);
	put(
		ObjectKind::Commit,
		encode_commit(&Commit {
			tree,
			parents: vec![],
			author: "A U Thor <a@x> 1700000000 +0000".to_owned(),
			committer: "A U Thor <a@x> 1700000000 +0000".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: "root\n".to_owned(),
		}),
	);

	objects
}

#[test]
fn our_index_matches_git_index_pack() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}

	let objects = sample_graph();
	let pack = encode_pack(&objects);

	// Let stock git index our pack; it writes `incoming.idx` next to the pack.
	let repo = TempDir::new().expect("tempdir");
	let repo_path = repo.path().join("repo");
	run_git(&[
		"init",
		"--object-format=sha256",
		"--bare",
		repo_path.to_str().unwrap(),
	]);
	let pack_path = repo.path().join("incoming.pack");
	std::fs::write(&pack_path, &pack).expect("write pack");
	let output = Command::new("git")
		.arg("-C")
		.arg(&repo_path)
		.args(["index-pack", "--object-format=sha256", "--strict"])
		.arg(&pack_path)
		.output()
		.expect("run git index-pack");
	assert!(
		output.status.success(),
		"git index-pack failed:\n{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let git_idx = std::fs::read(repo.path().join("incoming.idx")).expect("git wrote incoming.idx");

	// Our `.idx` for the same pack must match git's byte for byte.
	let entries = pack_index_entries::<Sha256>(&pack).expect("scan pack");
	let pack_checksum = &pack[pack.len() - 32..];
	let our_idx = encode_pack_index(&entries, pack_checksum).expect("encode our index");
	assert_eq!(
		our_idx, git_idx,
		"our .idx must be byte-identical to git index-pack's"
	);

	// And our reader parses git's `.idx` and agrees on every offset and CRC-32.
	let parsed = decode_pack_index::<Sha256>(&git_idx).expect("decode git's idx");
	assert_eq!(parsed.len(), objects.len());
	assert_eq!(parsed.pack_checksum(), pack_checksum);
	for entry in &entries {
		assert_eq!(parsed.offset_of(&entry.id), Some(entry.offset));
		assert_eq!(parsed.lookup(&entry.id).map(|e| e.crc32), Some(entry.crc32));
	}
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
