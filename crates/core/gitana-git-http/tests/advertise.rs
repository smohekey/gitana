//! Protocol-level tests for ref advertisement and `ls-refs` over an in-memory repo.
//! Byte-level stock-`git` interop belongs in higher-level HTTP integration tests.

use gitana_file_store_memory::MemoryFileStore;
use gitana_git_http::{ProtocolVersion, Service, advertise, ls_refs};
use gitana_object::{PktLine, parse_pkt};
use gitana_object_store::ObjectStore;
use gitana_repository::{FileMode, Repository, TreeBuildEntry};

fn repo() -> Repository<MemoryFileStore> {
	Repository::new(ObjectStore::new(MemoryFileStore::new()))
}

/// Init a repo and commit one file on `main`, returning the repo and the commit id.
async fn repo_with_commit() -> (Repository<MemoryFileStore>, String) {
	let repo = repo();
	repo.init().await.expect("init");
	let blob = repo.write_blob(b"hello\n").await.expect("blob");
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: "file.txt".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.expect("tree");
	let commit = repo
		.commit_on_head(tree, "A <a@x> 1 +0000", "A <a@x> 1 +0000", "root\n")
		.await
		.expect("commit");
	(repo, commit.to_hex())
}

/// Split an advertisement/result body into its decoded pkt-lines (control packets as
/// empty strings; data lines as their UTF-8 text).
fn pkt_lines(body: &[u8]) -> Vec<String> {
	let mut lines = Vec::new();
	let mut cursor = 0;
	while cursor < body.len() {
		let (line, consumed) = parse_pkt(&body[cursor..]).expect("pkt");
		cursor += consumed;
		match line {
			PktLine::Data(data) => lines.push(String::from_utf8_lossy(data).into_owned()),
			_ => lines.push(String::new()),
		}
	}
	lines
}

#[tokio::test]
async fn v2_advertisement_lists_capabilities() {
	let (repo, _) = repo_with_commit().await;
	let body = advertise(&repo, Service::UploadPack, ProtocolVersion::V2, None)
		.await
		.expect("advertise");
	let lines = pkt_lines(&body);

	assert_eq!(lines[0], "# service=git-upload-pack\n");
	assert!(lines.iter().any(|l| l == "version 2\n"));
	assert!(lines.iter().any(|l| l == "ls-refs=unborn\n"));
	assert!(lines.iter().any(|l| l == "object-format=sha256\n"));
}

#[tokio::test]
async fn v0_advertisement_lists_refs_with_caps_on_first_line() {
	let (repo, commit) = repo_with_commit().await;
	let body = advertise(&repo, Service::UploadPack, ProtocolVersion::V0, None)
		.await
		.expect("advertise");
	let lines = pkt_lines(&body);

	assert_eq!(lines[0], "# service=git-upload-pack\n");
	// First ref line is HEAD, carrying capabilities after a NUL.
	let first_ref = lines
		.iter()
		.find(|l| l.contains('\0'))
		.expect("a ref line with caps");
	assert!(first_ref.starts_with(&format!("{commit} HEAD\0")));
	assert!(first_ref.contains("object-format=sha256"));
	assert!(first_ref.contains("symref=HEAD:refs/heads/main"));
	// refs/heads/main is advertised on its own line.
	assert!(
		lines
			.iter()
			.any(|l| l == &format!("{commit} refs/heads/main\n"))
	);
}

#[tokio::test]
async fn v0_advertisement_for_empty_repo_emits_capabilities_placeholder() {
	let repo = repo();
	repo.init().await.expect("init");
	let body = advertise(&repo, Service::UploadPack, ProtocolVersion::V0, None)
		.await
		.expect("advertise");
	let lines = pkt_lines(&body);

	let placeholder = lines
		.iter()
		.find(|l| l.contains("capabilities^{}"))
		.expect("placeholder line");
	assert!(placeholder.starts_with(&format!("{} capabilities^{{}}\0", "0".repeat(64))));
	assert!(placeholder.contains("object-format=sha256"));
}

#[tokio::test]
async fn ls_refs_returns_refs_with_symref_target() {
	let (repo, commit) = repo_with_commit().await;

	// command=ls-refs, delim, symrefs + ref-prefix args, flush.
	let mut request = Vec::new();
	for line in ["command=ls-refs\n", "object-format=sha256\n"] {
		request.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
	}
	request.extend_from_slice(b"0001");
	for line in ["symrefs\n", "ref-prefix refs/\n", "ref-prefix HEAD\n"] {
		request.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
	}
	request.extend_from_slice(b"0000");

	let body = ls_refs(&repo, &request).await.expect("ls-refs");
	let lines = pkt_lines(&body);

	assert!(
		lines
			.iter()
			.any(|l| l == &format!("{commit} HEAD symref-target:refs/heads/main\n"))
	);
	assert!(
		lines
			.iter()
			.any(|l| l == &format!("{commit} refs/heads/main\n"))
	);
}

#[tokio::test]
async fn ls_refs_honors_ref_prefix_filter() {
	let (repo, commit) = repo_with_commit().await;

	let mut request = Vec::new();
	let cmd = "command=ls-refs\n";
	request.extend_from_slice(format!("{:04x}{cmd}", cmd.len() + 4).as_bytes());
	request.extend_from_slice(b"0001");
	let arg = "ref-prefix refs/tags/\n";
	request.extend_from_slice(format!("{:04x}{arg}", arg.len() + 4).as_bytes());
	request.extend_from_slice(b"0000");

	let body = ls_refs(&repo, &request).await.expect("ls-refs");
	let lines = pkt_lines(&body);

	// No tags exist and the prefix excludes HEAD / heads, so only the flush remains.
	assert!(!lines.iter().any(|l| l.contains(&commit)));
}
