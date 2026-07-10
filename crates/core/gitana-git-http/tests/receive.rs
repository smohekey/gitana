//! Protocol-level tests for `receive_pack` over an in-memory repo: a valid push moves
//! the ref and reports `ok`; a push whose objects are missing is rejected before any
//! ref moves. Stock-`git push` interop belongs in higher-level HTTP integration tests.

use std::sync::LazyLock;

use gitana_file_store_memory::MemoryFileStore;
use gitana_git_http::{NoReplayCheck, PushReflog, ReceiveOptions, TrustContext, receive_pack};
use gitana_object::Sha256;
use gitana_object::{
	Commit, ObjectId, ObjectKind, PackedObject, PktLine, TreeEntry, encode_commit, encode_pack,
	encode_tree, parse_pkt,
};
use gitana_object_store::ObjectStore;
use gitana_repository::{ReflogIntent, Repository};

const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A server committer line for the reflog-enabled push tests.
const COMMITTER: &str = "S E Rver <server@example.com> 1700000000 +0000";

/// These protocol tests do not configure trust, so every push runs against an empty trust context
/// (verification short-circuits to accept on a repo with no trust root).
static NO_TRUST: LazyLock<TrustContext> = LazyLock::new(TrustContext::none);

/// Receive options with `force` and no trust configuration — the shape these protocol tests push
/// with.
fn opts(force: bool) -> ReceiveOptions<'static, NoReplayCheck> {
	ReceiveOptions {
		force,
		trust: &NO_TRUST,
		now: 0,
		nonce_ledger: &NoReplayCheck,
		reflog: None,
	}
}

/// Like [`opts`], but crediting push reflogs to a fixed server identity — the shape a reflog-writing
/// server pushes with.
fn opts_with_reflog(force: bool) -> ReceiveOptions<'static, NoReplayCheck> {
	ReceiveOptions {
		reflog: Some(PushReflog {
			committer: COMMITTER,
		}),
		..opts(force)
	}
}

fn repo() -> Repository<MemoryFileStore, Sha256> {
	Repository::new(ObjectStore::<_, Sha256>::new(MemoryFileStore::new()))
}

/// Build a blob+tree+commit object set and return it with the commit id.
fn commit_objects(content: &[u8]) -> (Vec<PackedObject<Sha256>>, ObjectId<Sha256>) {
	let blob = content.to_vec();
	let blob_id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &blob);

	let tree = encode_tree(&[TreeEntry {
		mode: "100644".to_owned(),
		name: "file.txt".to_owned(),
		id: blob_id,
	}]);
	let tree_id = ObjectId::<Sha256>::compute(ObjectKind::Tree, &tree);

	let commit = encode_commit(&Commit {
		tree: tree_id,
		parents: vec![],
		author: "A <a@x> 1 +0000".to_owned(),
		committer: "A <a@x> 1 +0000".to_owned(),
		signature: None,
		extra_headers: Vec::new(),
		message: "root\n".to_owned(),
	});
	let commit_id = ObjectId::<Sha256>::compute(ObjectKind::Commit, &commit);

	let objects = vec![
		PackedObject {
			id: blob_id,
			kind: ObjectKind::Blob,
			data: blob,
		},
		PackedObject {
			id: tree_id,
			kind: ObjectKind::Tree,
			data: tree,
		},
		PackedObject {
			id: commit_id,
			kind: ObjectKind::Commit,
			data: commit,
		},
	];
	(objects, commit_id)
}

/// Frame `<old> <new> <ref>` (with caps on the first line) + flush + pack.
fn push_request(old: &str, new: &str, name: &str, pack: &[u8]) -> Vec<u8> {
	let command = format!("{old} {new} {name}\0report-status object-format=sha256\n");
	let mut out = Vec::new();
	out.extend_from_slice(format!("{:04x}{command}", command.len() + 4).as_bytes());
	out.extend_from_slice(b"0000");
	out.extend_from_slice(pack);
	out
}

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
async fn valid_push_moves_the_ref_and_reports_ok() {
	let repo = repo();
	repo.init().await.expect("init");

	let (objects, commit) = commit_objects(b"hello\n");
	let pack = encode_pack(&objects);
	let request = push_request(ZERO, &commit.to_hex(), "refs/heads/main", &pack);

	let response = receive_pack(&repo, &request, opts(false))
		.await
		.expect("receive")
		.report;
	let lines = pkt_lines(&response);
	assert!(lines.iter().any(|l| l == "unpack ok\n"), "{lines:?}");
	assert!(
		lines.iter().any(|l| l == "ok refs/heads/main\n"),
		"{lines:?}"
	);

	// The ref now resolves to the pushed commit, and the objects are stored.
	let resolved = repo
		.refs()
		.resolve("refs/heads/main")
		.await
		.expect("resolve");
	assert_eq!(resolved, Some(commit));
	assert_eq!(
		repo.read_blob(objects[0].id).await.expect("blob"),
		b"hello\n"
	);
}

#[tokio::test]
async fn push_with_missing_objects_is_rejected_without_moving_refs() {
	let repo = repo();
	repo.init().await.expect("init");

	// A command naming a commit, but an empty pack: connectivity fails.
	let (_, commit) = commit_objects(b"hello\n");
	let empty_pack = encode_pack::<Sha256>(&[]);
	let request = push_request(ZERO, &commit.to_hex(), "refs/heads/main", &empty_pack);

	let response = receive_pack(&repo, &request, opts(false))
		.await
		.expect("receive")
		.report;
	let lines = pkt_lines(&response);
	assert!(
		lines
			.iter()
			.any(|l| l.starts_with("unpack ") && l != "unpack ok\n"),
		"expected an unpack failure: {lines:?}"
	);
	assert!(
		lines.iter().any(|l| l.starts_with("ng refs/heads/main")),
		"{lines:?}"
	);
	// No ref was created.
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		None
	);
}

#[tokio::test]
async fn non_fast_forward_update_is_rejected() {
	let repo = repo();
	repo.init().await.expect("init");

	// First push establishes refs/heads/main.
	let (first, first_id) = commit_objects(b"one\n");
	let request = push_request(
		ZERO,
		&first_id.to_hex(),
		"refs/heads/main",
		&encode_pack(&first),
	);
	receive_pack(&repo, &request, opts(false))
		.await
		.expect("first push");

	// A second, unrelated root commit is not a descendant — a non-fast-forward update.
	let (second, second_id) = commit_objects(b"two\n");
	let request = push_request(
		&first_id.to_hex(),
		&second_id.to_hex(),
		"refs/heads/main",
		&encode_pack(&second),
	);
	let response = receive_pack(&repo, &request, opts(false))
		.await
		.expect("second push")
		.report;
	let lines = pkt_lines(&response);
	assert!(lines.iter().any(|l| l == "unpack ok\n"));
	assert!(
		lines
			.iter()
			.any(|l| l.contains("ng refs/heads/main non-fast-forward")),
		"{lines:?}"
	);
	// The ref still points at the first commit.
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		Some(first_id)
	);
}

#[tokio::test]
async fn delete_with_force_removes_the_ref() {
	let repo = repo();
	repo.init().await.expect("init");

	let (objects, commit) = commit_objects(b"one\n");
	let create = push_request(
		ZERO,
		&commit.to_hex(),
		"refs/heads/main",
		&encode_pack(&objects),
	);
	receive_pack(&repo, &create, opts(false))
		.await
		.expect("create");

	// Delete it (new = zero) — allowed with force.
	let delete = push_request(
		&commit.to_hex(),
		ZERO,
		"refs/heads/main",
		&encode_pack::<Sha256>(&[]),
	);
	let report = receive_pack(&repo, &delete, opts(true))
		.await
		.expect("delete")
		.report;
	let lines = pkt_lines(&report);
	assert!(lines.iter().any(|l| l == "unpack ok\n"), "{lines:?}");
	assert!(
		lines.iter().any(|l| l == "ok refs/heads/main\n"),
		"{lines:?}"
	);
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		None,
		"the ref is gone after a forced delete"
	);
}

#[tokio::test]
async fn delete_without_force_is_denied() {
	let repo = repo();
	repo.init().await.expect("init");

	let (objects, commit) = commit_objects(b"one\n");
	let create = push_request(
		ZERO,
		&commit.to_hex(),
		"refs/heads/main",
		&encode_pack(&objects),
	);
	receive_pack(&repo, &create, opts(false))
		.await
		.expect("create");

	let delete = push_request(
		&commit.to_hex(),
		ZERO,
		"refs/heads/main",
		&encode_pack::<Sha256>(&[]),
	);
	let report = receive_pack(&repo, &delete, opts(false))
		.await
		.expect("delete attempt")
		.report;
	let lines = pkt_lines(&report);
	assert!(
		lines
			.iter()
			.any(|l| l.starts_with("ng refs/heads/main") && l.contains("deletion denied")),
		"{lines:?}"
	);
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		Some(commit),
		"a denied delete leaves the ref untouched"
	);
}

#[tokio::test]
async fn force_push_allows_a_non_fast_forward_update() {
	let repo = repo();
	repo.init().await.expect("init");

	let (first, first_id) = commit_objects(b"one\n");
	let create = push_request(
		ZERO,
		&first_id.to_hex(),
		"refs/heads/main",
		&encode_pack(&first),
	);
	receive_pack(&repo, &create, opts(false))
		.await
		.expect("first push");

	// An unrelated second root commit — a non-fast-forward update, allowed with force.
	let (second, second_id) = commit_objects(b"two\n");
	let force = push_request(
		&first_id.to_hex(),
		&second_id.to_hex(),
		"refs/heads/main",
		&encode_pack(&second),
	);
	let report = receive_pack(&repo, &force, opts(true))
		.await
		.expect("force push")
		.report;
	let lines = pkt_lines(&report);
	assert!(
		lines.iter().any(|l| l == "ok refs/heads/main\n"),
		"{lines:?}"
	);
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		Some(second_id),
		"force-push moved the ref to the unrelated commit"
	);
}

/// A create that loses the CAS race — the ref already exists at the pushed tip (a concurrent push
/// won) — is reported `ng` without disturbing the winner's ref. The reflog-failure rollback must fire
/// only for a genuine post-CAS reflog failure, never for a `RefMoved`, or concurrent identical pushes
/// would undo each other.
#[tokio::test]
async fn losing_create_race_does_not_roll_back_the_winner() {
	let repo = repo();
	repo.init().await.expect("init");

	let (objects, commit) = commit_objects(b"hello\n");
	// A concurrent winner has already created the ref at `commit`.
	repo
		.refs()
		.update_ref("refs/heads/main", commit, None, ReflogIntent::Skip)
		.await
		.expect("pre-create");

	// Our create (old=zero) collides with the existing ref: `update_ref` returns `RefMoved` before
	// writing anything. With reflogs enabled, the rollback must recognise this as a lost race and
	// leave the winner's ref in place — not treat `resolve == new` as its own move and delete it.
	let request = push_request(
		ZERO,
		&commit.to_hex(),
		"refs/heads/main",
		&encode_pack(&objects),
	);
	let report = receive_pack(&repo, &request, opts_with_reflog(false))
		.await
		.expect("receive")
		.report;
	let lines = pkt_lines(&report);
	assert!(
		lines.iter().any(|l| l.starts_with("ng refs/heads/main")),
		"{lines:?}"
	);
	assert_eq!(
		repo
			.refs()
			.resolve("refs/heads/main")
			.await
			.expect("resolve"),
		Some(commit),
		"the winner's ref must survive a losing create race"
	);
}
