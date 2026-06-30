//! Protocol-level tests for v2 `fetch` and v0 upload-pack over an in-memory repo:
//! the negotiation sections and that the side-band packfile decodes back to the
//! requested objects. Stock-`git clone` interop belongs in higher-level HTTP
//! integration tests.

use gitana_file_store_memory::MemoryFileStore;
use gitana_git_http::{fetch, upload_pack_v0};
use gitana_object::Sha256;
use gitana_object::{ObjectId, PktLine, decode_pack, parse_pkt};
use gitana_object_store::ObjectStore;
use gitana_repository::{FileMode, Repository, TreeBuildEntry};

fn repo() -> Repository<MemoryFileStore, Sha256> {
	Repository::new(ObjectStore::<_, Sha256>::new(MemoryFileStore::new()))
}

/// Init a repo and commit one file on `main`, returning the repo and commit id.
async fn repo_with_commit() -> (Repository<MemoryFileStore, Sha256>, ObjectId<Sha256>) {
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
	(repo, commit)
}

/// Frame a pkt-line with the given data.
fn pkt(data: &str) -> Vec<u8> {
	let mut out = Vec::new();
	out.extend_from_slice(format!("{:04x}{data}", data.len() + 4).as_bytes());
	out
}

/// Split a response into decoded pkt-lines: control packets as empty strings, data
/// lines as raw bytes.
fn pkt_lines(body: &[u8]) -> Vec<Vec<u8>> {
	let mut lines = Vec::new();
	let mut cursor = 0;
	while cursor < body.len() {
		let (line, consumed) = parse_pkt(&body[cursor..]).expect("pkt");
		cursor += consumed;
		match line {
			PktLine::Data(data) => lines.push(data.to_vec()),
			_ => lines.push(Vec::new()),
		}
	}
	lines
}

/// Reassemble channel-1 side-band data from the `packfile` section of a response.
fn extract_pack(body: &[u8]) -> Vec<u8> {
	let lines = pkt_lines(body);
	let start = lines
		.iter()
		.position(|l| l == b"packfile\n")
		.expect("packfile section");
	let mut pack = Vec::new();
	for line in &lines[start + 1..] {
		if let Some((&channel, rest)) = line.split_first()
			&& channel == 1
		{
			pack.extend_from_slice(rest);
		}
	}
	pack
}

#[tokio::test]
async fn fetch_with_done_returns_a_pack_of_the_wants() {
	let (repo, commit) = repo_with_commit().await;

	let mut request = pkt("command=fetch\n");
	request.extend_from_slice(b"0001");
	request.extend_from_slice(&pkt(&format!("want {commit}\n")));
	request.extend_from_slice(&pkt("done\n"));
	request.extend_from_slice(b"0000");

	let response = fetch(&repo, &request).await.expect("fetch");
	let pack = extract_pack(&response);
	let objects = decode_pack::<Sha256>(&pack).expect("decode");
	// commit + its tree + the blob.
	assert_eq!(objects.len(), 3);
	assert!(objects.iter().any(|o| o.id == commit));
}

#[tokio::test]
async fn fetch_without_haves_acks_nak() {
	let (repo, commit) = repo_with_commit().await;

	// No `done`, and a `have` the server lacks: it cannot find a cut point yet.
	let phantom = ObjectId::<Sha256>::compute(gitana_object::ObjectKind::Commit, b"absent");
	let mut request = pkt("command=fetch\n");
	request.extend_from_slice(b"0001");
	request.extend_from_slice(&pkt(&format!("want {commit}\n")));
	request.extend_from_slice(&pkt(&format!("have {phantom}\n")));
	request.extend_from_slice(b"0000");

	let response = fetch(&repo, &request).await.expect("fetch");
	let lines = pkt_lines(&response);
	assert_eq!(lines[0], b"acknowledgments\n");
	assert!(lines.iter().any(|l| l == b"NAK\n"));
	assert!(!lines.iter().any(|l| l == b"packfile\n"));
}

#[tokio::test]
async fn fetch_excludes_objects_reachable_from_haves() {
	let (repo, first) = repo_with_commit().await;
	// Second commit; the client already has the first.
	let blob = repo.write_blob(b"second\n").await.expect("blob");
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: "file.txt".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.expect("tree");
	let second = repo
		.commit_on_head(tree, "A <a@x> 1 +0000", "A <a@x> 1 +0000", "update\n")
		.await
		.expect("commit");

	let mut request = pkt("command=fetch\n");
	request.extend_from_slice(b"0001");
	request.extend_from_slice(&pkt(&format!("want {second}\n")));
	request.extend_from_slice(&pkt(&format!("have {first}\n")));
	request.extend_from_slice(&pkt("done\n"));
	request.extend_from_slice(b"0000");

	let response = fetch(&repo, &request).await.expect("fetch");
	let objects = decode_pack::<Sha256>(&extract_pack(&response)).expect("decode");
	let ids: Vec<ObjectId<Sha256>> = objects.iter().map(|o| o.id).collect();
	// Only the new commit, its tree, and the new blob — not the first commit.
	assert!(ids.contains(&second));
	assert!(!ids.contains(&first));
	assert_eq!(objects.len(), 3);
}

#[tokio::test]
async fn v0_upload_pack_returns_nak_then_pack() {
	let (repo, commit) = repo_with_commit().await;

	let mut request = pkt(&format!("want {commit} side-band-64k ofs-delta\n"));
	request.extend_from_slice(b"0000");
	request.extend_from_slice(&pkt("done\n"));

	let response = upload_pack_v0(&repo, &request).await.expect("v0");
	let lines = pkt_lines(&response);
	assert_eq!(lines[0], b"NAK\n");
	let objects = decode_pack::<Sha256>(&extract_pack_v0(&response)).expect("decode");
	assert!(objects.iter().any(|o| o.id == commit));
}

/// Reassemble the side-band pack from a v0 response (NAK then channel-1 lines).
fn extract_pack_v0(body: &[u8]) -> Vec<u8> {
	let mut pack = Vec::new();
	for line in pkt_lines(body) {
		if let Some((&channel, rest)) = line.split_first()
			&& channel == 1
		{
			pack.extend_from_slice(rest);
		}
	}
	pack
}
