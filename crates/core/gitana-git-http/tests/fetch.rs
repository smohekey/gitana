//! Protocol-level tests for v2 `fetch` and v0 upload-pack over an in-memory repo:
//! the negotiation sections and that the side-band packfile decodes back to the
//! requested objects. Stock-`git clone` interop belongs in higher-level HTTP
//! integration tests.

use std::collections::HashMap;

use gitana_file_store_memory::MemoryFileStore;
use gitana_git_http::{fetch, upload_pack_v0};
use gitana_object::Sha256;
use gitana_object::{
	ObjectId, PktLine, decode_pack, decode_pack_with_bases, parse_pkt, ref_delta_base_ids,
};
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

// --- thin packs ---------------------------------------------------------------------

/// A large, delta-friendly blob tagged by `marker`, so a successor differing by a byte
/// deltas well against it.
fn big_blob(marker: u8) -> Vec<u8> {
	let mut data = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
	data.push(marker);
	data
}

/// A repo with two commits to `file.txt`: a big blob, then a near-identical one. Returns
/// the repo, both commit ids, and the second (new) blob's id.
async fn repo_with_two_big_commits() -> (
	Repository<MemoryFileStore, Sha256>,
	ObjectId<Sha256>,
	ObjectId<Sha256>,
	ObjectId<Sha256>,
) {
	let repo = repo();
	repo.init().await.expect("init");
	let commit_file = async |content: &[u8], msg: &str| {
		let blob = repo.write_blob(content).await.expect("blob");
		let tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "file.txt".to_owned(),
				mode: FileMode::Regular,
				id: blob,
			}])
			.await
			.expect("tree");
		let commit = repo
			.commit_on_head(tree, "A <a@x> 1 +0000", "A <a@x> 1 +0000", msg)
			.await
			.expect("commit");
		(blob, commit)
	};
	let (_, first) = commit_file(&big_blob(b'A'), "first\n").await;
	let (second_blob, second) = commit_file(&big_blob(b'B'), "second\n").await;
	(repo, first, second, second_blob)
}

/// Complete a (possibly thin) pack against the repo's object store — the client-side
/// de-thinning: supply every referenced base we already have, then decode.
async fn complete_against_store(
	repo: &Repository<MemoryFileStore, Sha256>,
	pack: &[u8],
) -> Vec<gitana_object::PackedObject<Sha256>> {
	let mut bases = HashMap::new();
	for id in ref_delta_base_ids::<Sha256>(pack).expect("scan bases") {
		if let Ok((kind, data)) = repo.objects().read_object(&id).await {
			bases.insert(id, (kind, data));
		}
	}
	decode_pack_with_bases::<Sha256>(pack, &bases).expect("complete")
}

/// Build a v2 `fetch` body wanting `want`, having `have`, optionally negotiating thin.
fn v2_fetch_body(want: &ObjectId<Sha256>, have: &ObjectId<Sha256>, thin: bool) -> Vec<u8> {
	let mut request = pkt("command=fetch\n");
	request.extend_from_slice(b"0001");
	if thin {
		request.extend_from_slice(&pkt("thin-pack\n"));
	}
	request.extend_from_slice(&pkt(&format!("want {want}\n")));
	request.extend_from_slice(&pkt(&format!("have {have}\n")));
	request.extend_from_slice(&pkt("done\n"));
	request.extend_from_slice(b"0000");
	request
}

#[tokio::test]
async fn v2_fetch_serves_a_thin_pack_only_when_negotiated() {
	let (repo, first, second, second_blob) = repo_with_two_big_commits().await;

	// Without `thin-pack`, the pack is self-contained: it decodes standalone.
	let fat = extract_pack(
		&fetch(&repo, &v2_fetch_body(&second, &first, false))
			.await
			.unwrap(),
	);
	assert!(
		ref_delta_base_ids::<Sha256>(&fat).unwrap().is_empty(),
		"a fat pack must carry no external REF-delta bases"
	);
	decode_pack::<Sha256>(&fat).expect("fat pack decodes standalone");

	// With `thin-pack`, the new blob is a REF delta against the old one (not carried), so
	// a standalone decode fails but completing against the store yields the new blob.
	let thin = extract_pack(
		&fetch(&repo, &v2_fetch_body(&second, &first, true))
			.await
			.unwrap(),
	);
	assert!(
		!ref_delta_base_ids::<Sha256>(&thin).unwrap().is_empty(),
		"a thin pack must reference an external base"
	);
	assert!(
		thin.len() < fat.len(),
		"the thin pack ({}) should be smaller than the fat one ({})",
		thin.len(),
		fat.len()
	);
	assert!(
		decode_pack::<Sha256>(&thin).is_err(),
		"thin pack is not self-contained"
	);
	let completed = complete_against_store(&repo, &thin).await;
	assert!(
		completed.iter().any(|o| o.id == second_blob),
		"new blob resolved"
	);
	assert!(
		completed.iter().any(|o| o.id == second),
		"new commit present"
	);
}

#[tokio::test]
async fn v0_upload_pack_serves_a_thin_pack_only_when_negotiated() {
	let (repo, first, second, second_blob) = repo_with_two_big_commits().await;

	let body = |thin: bool| {
		let caps = if thin {
			"side-band-64k thin-pack ofs-delta"
		} else {
			"side-band-64k ofs-delta"
		};
		let mut request = pkt(&format!("want {second} {caps}\n"));
		request.extend_from_slice(&pkt(&format!("want {second}\n")));
		request.extend_from_slice(b"0000");
		request.extend_from_slice(&pkt(&format!("have {first}\n")));
		request.extend_from_slice(&pkt("done\n"));
		request
	};

	let fat = extract_pack_v0(&upload_pack_v0(&repo, &body(false)).await.unwrap());
	assert!(ref_delta_base_ids::<Sha256>(&fat).unwrap().is_empty());
	decode_pack::<Sha256>(&fat).expect("fat pack decodes standalone");

	let thin = extract_pack_v0(&upload_pack_v0(&repo, &body(true)).await.unwrap());
	assert!(!ref_delta_base_ids::<Sha256>(&thin).unwrap().is_empty());
	assert!(decode_pack::<Sha256>(&thin).is_err());
	let completed = complete_against_store(&repo, &thin).await;
	assert!(completed.iter().any(|o| o.id == second_blob));
	assert!(completed.iter().any(|o| o.id == second));
}
