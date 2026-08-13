#![cfg(not(target_arch = "wasm32"))]

use std::{
	io::Write,
	process::{Command, Stdio},
};

use gitana_object::{Sha256, validate_commit_structure};

const EMPTY_TREE: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

#[test]
fn structural_validation_matches_stock_git_for_regressions() {
	let valid = format!(
		"tree {EMPTY_TREE}\nauthor A <a@x> 1 +0000\n\
		 committer C <c@x> 2 +0000\n\nmessage\n"
	);
	let duplicate_tree = format!(
		"tree {EMPTY_TREE}\ntree {EMPTY_TREE}\nauthor A <a@x> 1 +0000\n\
		 committer C <c@x> 2 +0000\n\nmessage\n"
	);
	let negative_date = format!(
		"tree {EMPTY_TREE}\nauthor A <a@x> -1 +0000\n\
		 committer C <c@x> 2 +0000\n\nmessage\n"
	);

	validate_commit_structure::<Sha256>(valid.as_bytes()).expect("valid fixture");
	assert!(validate_commit_structure::<Sha256>(duplicate_tree.as_bytes()).is_err());
	assert!(validate_commit_structure::<Sha256>(negative_date.as_bytes()).is_err());
	assert!(git_fsck_accepts(valid.as_bytes()));
	assert!(!git_fsck_accepts(duplicate_tree.as_bytes()));
	assert!(!git_fsck_accepts(negative_date.as_bytes()));
}

fn git_fsck_accepts(commit: &[u8]) -> bool {
	let directory = tempfile::tempdir().expect("temporary repository");
	let git_dir = directory.path().join("repo.git");
	assert!(
		Command::new("git")
			.args(["init", "--bare", "--object-format=sha256"])
			.arg(&git_dir)
			.output()
			.expect("git init")
			.status
			.success()
	);
	write_object(&git_dir, "tree", b"");
	let commit = write_object(&git_dir, "commit", commit);
	Command::new("git")
		.arg(format!("--git-dir={}", git_dir.display()))
		.args(["fsck", "--strict", &commit])
		.output()
		.expect("git fsck")
		.status
		.success()
}

fn write_object(git_dir: &std::path::Path, kind: &str, payload: &[u8]) -> String {
	let mut child = Command::new("git")
		.arg(format!("--git-dir={}", git_dir.display()))
		.args(["hash-object", "-t", kind, "--literally", "-w", "--stdin"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.spawn()
		.expect("git hash-object");
	child
		.stdin
		.take()
		.expect("hash-object stdin")
		.write_all(payload)
		.expect("write object payload");
	let output = child.wait_with_output().expect("hash-object output");
	assert!(output.status.success());
	String::from_utf8(output.stdout)
		.expect("object id is UTF-8")
		.trim()
		.to_owned()
}
