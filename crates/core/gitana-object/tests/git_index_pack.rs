//! Differential test: a pack produced by [`gitana_object::encode_pack`] is accepted
//! by stock `git index-pack --object-format=sha256` and round-trips through
//! [`gitana_object::decode_pack`].
//!
//! # Running
//!
//! Requires a `git` on `PATH` that supports SHA-256 repositories (git >= 2.29).
//! Where it does not, the git-oracle assertions are skipped with a printed note;
//! the decode round-trip still runs.

use std::collections::HashSet;
use std::process::Command;

use gitana_object::{
	Commit, ObjectId, ObjectKind, PackedObject, Sha256, TreeEntry, decode_pack, encode_commit,
	encode_pack, encode_tree, enumerate_objects,
};
use tempfile::TempDir;

/// Build a small but realistic object graph: a root commit and a child commit, two
/// versions of a file (so the blobs are delta-friendly), and their trees.
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
	let blob2 = put(ObjectKind::Blob, body2);

	let tree1 = put(
		ObjectKind::Tree,
		encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: "file.txt".to_owned(),
			id: blob1,
		}]),
	);
	let tree2 = put(
		ObjectKind::Tree,
		encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: "file.txt".to_owned(),
			id: blob2,
		}]),
	);

	let root = put(
		ObjectKind::Commit,
		encode_commit(&Commit {
			tree: tree1,
			parents: vec![],
			author: "A U Thor <a@x> 1700000000 +0000".to_owned(),
			committer: "A U Thor <a@x> 1700000000 +0000".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: "root\n".to_owned(),
		}),
	);
	put(
		ObjectKind::Commit,
		encode_commit(&Commit {
			tree: tree2,
			parents: vec![root],
			author: "A U Thor <a@x> 1700000100 +0000".to_owned(),
			committer: "A U Thor <a@x> 1700000100 +0000".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: "second\n".to_owned(),
		}),
	);

	objects
}

#[test]
fn produced_pack_round_trips_through_decode() {
	let objects = sample_graph();
	let pack = encode_pack(&objects);

	let decoded = decode_pack::<Sha256>(&pack).expect("decode our own pack");
	let want: HashSet<ObjectId<Sha256>> = objects.iter().map(|o| o.id).collect();
	let got: HashSet<ObjectId<Sha256>> = decoded.iter().map(|o| o.id).collect();
	assert_eq!(got, want);
	// Payloads survive intact (ids already cover this, but assert directly too).
	for original in &objects {
		let back = decoded
			.iter()
			.find(|o| o.id == original.id)
			.expect("object present");
		assert_eq!(&back.data, &original.data);
		assert_eq!(back.kind, original.kind);
	}
}

#[test]
fn enumerated_pack_round_trips() {
	// Drive the full pipeline: enumerate from the tip, encode, decode.
	let objects = sample_graph();
	let tip = objects.last().expect("non-empty").id;
	let by_id: std::collections::HashMap<ObjectId<Sha256>, PackedObject<Sha256>> =
		objects.iter().cloned().map(|o| (o.id, o)).collect();

	let enumerated = enumerate_objects(&[tip], &[], |id| {
		Ok(by_id.get(&id).map(|o| (o.kind, o.data.clone())))
	})
	.expect("enumerate");
	assert_eq!(enumerated.len(), objects.len());

	let pack = encode_pack(&enumerated);
	let decoded = decode_pack::<Sha256>(&pack).expect("decode");
	assert_eq!(decoded.len(), objects.len());
}

#[test]
fn git_index_pack_accepts_our_pack() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}

	let objects = sample_graph();
	let pack = encode_pack(&objects);

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

	// index-pack parses the pack, resolves every delta, and recomputes each object's
	// SHA-256 id — it fails non-zero if anything about the pack is malformed.
	let output = Command::new("git")
		.arg("-C")
		.arg(&repo_path)
		.args(["index-pack", "--object-format=sha256", "--strict", "-v"])
		.arg(&pack_path)
		.output()
		.expect("run git index-pack");
	assert!(
		output.status.success(),
		"git index-pack rejected our pack:\n{}",
		String::from_utf8_lossy(&output.stderr)
	);
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
