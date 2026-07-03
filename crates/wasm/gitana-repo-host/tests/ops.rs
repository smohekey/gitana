//! Targeted single-format tests for op semantics the generic roundtrip does not
//! cover: the CAS matrix, HEAD states, and typed error variants.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use gitana_object::{ObjectId, ObjectKind, Sha256};
use gitana_repo_host::exports::gitana::repo::porcelain::{HeadState, RepoError};

use self::support::{Session, build_fixture, native_repo};

#[tokio::test]
async fn cas_matrix_and_packed_delete() -> Result<()> {
	let fixture = build_fixture::<Sha256>().await?;
	let git_dir = fixture.dir.path();
	let mut session = Session::open(git_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// Create requires absence…
	porcelain
		.call_update_ref(&mut *store, handle, "refs/heads/guest", "main", None)
		.await?
		.map_err(|error| anyhow!("create: {error:?}"))?;
	// …so a second create collides.
	let collide = porcelain
		.call_update_ref(&mut *store, handle, "refs/heads/guest", "main", None)
		.await?;
	assert!(
		matches!(collide, Err(RepoError::RefMoved(_))),
		"{collide:?}"
	);

	// A wrong expected value is a CAS failure; a malformed one is invalid input.
	let wrong = porcelain
		.call_update_ref(
			&mut *store,
			handle,
			"refs/heads/guest",
			"feature",
			Some(&fixture.a),
		)
		.await?;
	assert!(matches!(wrong, Err(RepoError::RefMoved(_))), "{wrong:?}");
	let malformed = porcelain
		.call_update_ref(
			&mut *store,
			handle,
			"refs/heads/guest",
			"feature",
			Some("main"),
		)
		.await?;
	assert!(
		matches!(malformed, Err(RepoError::Invalid(_))),
		"{malformed:?}"
	);

	// The correct expected value moves the ref (`new` is a spec).
	porcelain
		.call_update_ref(
			&mut *store,
			handle,
			"refs/heads/guest",
			"feature",
			Some(&fixture.m),
		)
		.await?
		.map_err(|error| anyhow!("update: {error:?}"))?;
	let moved = porcelain
		.call_resolve_ref(&mut *store, handle, "refs/heads/guest")
		.await?
		.map_err(|error| anyhow!("resolve: {error:?}"))?;
	assert_eq!(moved, Some(fixture.d.clone()));

	// Delete is CAS too.
	let wrong_delete = porcelain
		.call_delete_ref(&mut *store, handle, "refs/heads/guest", &fixture.m)
		.await?;
	assert!(
		matches!(wrong_delete, Err(RepoError::RefMoved(_))),
		"{wrong_delete:?}"
	);
	porcelain
		.call_delete_ref(&mut *store, handle, "refs/heads/guest", &fixture.d)
		.await?
		.map_err(|error| anyhow!("delete: {error:?}"))?;
	let gone = porcelain
		.call_resolve_ref(&mut *store, handle, "refs/heads/guest")
		.await?
		.map_err(|error| anyhow!("resolve deleted: {error:?}"))?;
	assert_eq!(gone, None);

	// A packed-only ref participates in CAS: create-over is refused, its packed
	// value is the compare value, and an update writes the shadowing loose file.
	let packed_create = porcelain
		.call_update_ref(&mut *store, handle, "refs/heads/packed", "main", None)
		.await?;
	assert!(
		matches!(packed_create, Err(RepoError::RefMoved(_))),
		"{packed_create:?}"
	);
	porcelain
		.call_update_ref(
			&mut *store,
			handle,
			"refs/heads/packed",
			"feature",
			Some(&fixture.a),
		)
		.await?
		.map_err(|error| anyhow!("update packed: {error:?}"))?;
	let shadowed = porcelain
		.call_resolve_ref(&mut *store, handle, "refs/heads/packed")
		.await?
		.map_err(|error| anyhow!("resolve packed: {error:?}"))?;
	assert_eq!(shadowed, Some(fixture.d.clone()));

	// Deleting it removes both the loose shadow and the packed-refs line.
	porcelain
		.call_delete_ref(&mut *store, handle, "refs/heads/packed", &fixture.d)
		.await?
		.map_err(|error| anyhow!("delete packed: {error:?}"))?;
	let listed = porcelain
		.call_list_refs(&mut *store, handle, "refs/heads/")
		.await?
		.map_err(|error| anyhow!("list-refs: {error:?}"))?;
	assert!(listed.iter().all(|entry| entry.name != "refs/heads/packed"));
	let packed = std::fs::read_to_string(git_dir.join("packed-refs"))?;
	assert!(!packed.contains("refs/heads/packed"));

	Ok(())
}

#[tokio::test]
async fn head_states() -> Result<()> {
	let fixture = build_fixture::<Sha256>().await?;
	let git_dir = fixture.dir.path();

	// A natively-written detached HEAD is seen as such by the guest.
	std::fs::write(git_dir.join("HEAD"), format!("{}\n", fixture.m))?;
	let mut session = Session::open(git_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;
	match porcelain
		.call_head(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("head: {error:?}"))?
	{
		HeadState::Detached(id) => assert_eq!(id, fixture.m),
		other => bail!("expected detached HEAD, got {other:?}"),
	}

	// The guest moves HEAD back onto an existing branch; native gitana agrees.
	porcelain
		.call_set_symbolic_ref(&mut *store, handle, "HEAD", "refs/heads/feature")
		.await?
		.map_err(|error| anyhow!("set-symbolic-ref: {error:?}"))?;
	match porcelain
		.call_head(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("head: {error:?}"))?
	{
		HeadState::Symbolic(symbolic) => {
			assert_eq!(symbolic.target, "refs/heads/feature");
			assert_eq!(symbolic.id, fixture.d);
		}
		other => bail!("expected symbolic HEAD, got {other:?}"),
	}
	let native = native_repo::<Sha256>(git_dir)?;
	assert_eq!(
		native.refs().read_symbolic("HEAD").await?.as_deref(),
		Some("refs/heads/feature")
	);

	// HEAD at a not-yet-existing branch is unborn.
	porcelain
		.call_set_symbolic_ref(&mut *store, handle, "HEAD", "refs/heads/unborn")
		.await?
		.map_err(|error| anyhow!("set-symbolic-ref unborn: {error:?}"))?;
	match porcelain
		.call_head(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("head: {error:?}"))?
	{
		HeadState::Unborn(target) => assert_eq!(target, "refs/heads/unborn"),
		other => bail!("expected unborn HEAD, got {other:?}"),
	}

	Ok(())
}

#[tokio::test]
async fn typed_revision_errors() -> Result<()> {
	let fixture = build_fixture::<Sha256>().await?;
	let git_dir = fixture.dir.path();
	let native = native_repo::<Sha256>(git_dir)?;

	// Manufacture two loose blobs sharing a 4-hex id prefix (abbreviation
	// resolution scans loose objects, so this must run pre-repack).
	let mut buckets: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
	let mut prefix = None;
	for i in 0u64.. {
		let content = format!("collide {i}\n").into_bytes();
		let hex = ObjectId::<Sha256>::compute(ObjectKind::Blob, &content).to_hex();
		let bucket = buckets.entry(hex[..4].to_owned()).or_default();
		bucket.push(content);
		if bucket.len() == 2 {
			prefix = Some(hex[..4].to_owned());
			break;
		}
	}
	let prefix = prefix.expect("a colliding 4-hex prefix");
	for content in &buckets[&prefix] {
		native.write_blob(content).await?;
	}

	let mut session = Session::open(git_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	let ambiguous = porcelain
		.call_rev_parse(&mut *store, handle, &prefix)
		.await?;
	assert!(
		matches!(ambiguous, Err(RepoError::Ambiguous(_))),
		"{ambiguous:?}"
	);

	let unknown = porcelain
		.call_rev_parse(&mut *store, handle, "nope")
		.await?;
	assert!(
		matches!(unknown, Err(RepoError::UnknownRevision(_))),
		"{unknown:?}"
	);
	let unknown_full = porcelain
		.call_rev_parse(&mut *store, handle, &"0".repeat(64))
		.await?;
	assert!(
		matches!(unknown_full, Err(RepoError::UnknownRevision(_))),
		"{unknown_full:?}"
	);

	let invalid = porcelain
		.call_rev_parse(&mut *store, handle, "main^{banana}")
		.await?;
	assert!(matches!(invalid, Err(RepoError::Invalid(_))), "{invalid:?}");

	Ok(())
}

#[tokio::test]
async fn init_creates_a_fresh_repository() -> Result<()> {
	use gitana_repo_host::exports::gitana::repo::porcelain::{FileMode, HashKind, TreeBuildEntry};

	for (kind, other) in [
		(HashKind::Sha256, HashKind::Sha1),
		(HashKind::Sha1, HashKind::Sha256),
	] {
		let tmp = tempfile::tempdir()?;
		let git_dir = tmp.path();
		let mut session = Session::init(git_dir, kind).await?;
		let porcelain = session.repo.gitana_repo_porcelain().repository();
		let store = &mut session.store;
		let handle = session.handle;

		assert_eq!(porcelain.call_hash_kind(&mut *store, handle).await?, kind);
		match porcelain
			.call_head(&mut *store, handle)
			.await?
			.map_err(|error| anyhow!("head: {error:?}"))?
		{
			HeadState::Unborn(target) => assert_eq!(target, "refs/heads/main"),
			state => bail!("expected unborn HEAD, got {state:?}"),
		}
		// git's skeleton exists on the host filesystem.
		for dir in [
			"objects/pack",
			"objects/info",
			"refs/heads",
			"refs/tags",
			"info",
		] {
			assert!(git_dir.join(dir).is_dir(), "{dir} missing");
		}

		// The guest can build history in its own repository.
		let blob = porcelain
			.call_write_blob(&mut *store, handle, b"born in wasm\n")
			.await?
			.map_err(|error| anyhow!("write-blob: {error:?}"))?;
		let tree = porcelain
			.call_write_tree(
				&mut *store,
				handle,
				&[TreeBuildEntry {
					path: "file.txt".to_owned(),
					mode: FileMode::Regular,
					id: blob,
				}],
			)
			.await?
			.map_err(|error| anyhow!("write-tree: {error:?}"))?;
		let commit = porcelain
			.call_create_commit(
				&mut *store,
				handle,
				&tree,
				&[],
				support::AUTHOR,
				&support::committer(0),
				"first wasm commit\n",
			)
			.await?
			.map_err(|error| anyhow!("create-commit: {error:?}"))?;
		porcelain
			.call_update_ref(&mut *store, handle, "refs/heads/main", &commit, None)
			.await?
			.map_err(|error| anyhow!("update-ref: {error:?}"))?;
		match porcelain
			.call_head(&mut *store, handle)
			.await?
			.map_err(|error| anyhow!("head: {error:?}"))?
		{
			HeadState::Symbolic(symbolic) => assert_eq!(symbolic.id, commit),
			state => bail!("expected symbolic HEAD after the first commit, got {state:?}"),
		}

		// Re-init with the same kind is a no-op; the other kind is refused.
		drop(session);
		let again = Session::init(git_dir, kind).await?;
		drop(again);
		let refused = Session::try_init(git_dir, other).await?;
		match refused {
			Err(RepoError::UnsupportedFormat(_)) => {}
			Err(error) => bail!("expected unsupported-format, got {error:?}"),
			Ok(_) => bail!("expected re-init with {other:?} to be refused"),
		}
	}

	Ok(())
}

#[tokio::test]
async fn repack_then_read_through_the_descriptor_backend() -> Result<()> {
	let fixture = build_fixture::<Sha256>().await?;
	let git_dir = fixture.dir.path();
	let mut session = Session::open(git_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// Everything in the fixture is loose; a full repack consolidates it.
	let report = porcelain
		.call_repack(&mut *store, handle, false)
		.await?
		.map_err(|error| anyhow!("repack: {error:?}"))?
		.expect("a fixture full of loose objects repacks");
	assert!(report.packed_objects > 0, "{report:?}");
	assert!(report.packs_written >= 1, "{report:?}");
	assert!(report.loose_removed > 0, "{report:?}");
	assert!(
		git_dir.join("objects/pack").read_dir()?.next().is_some(),
		"a pack exists on disk"
	);

	// The spike only proved loose-object reads; these now go through the pack
	// (and multi-pack-index) via the descriptor backend.
	let commit = porcelain
		.call_read_commit(&mut *store, handle, "HEAD")
		.await?
		.map_err(|error| anyhow!("read-commit post-repack: {error:?}"))?;
	assert_eq!(commit.id, fixture.m);
	let blob = porcelain
		.call_read_blob(&mut *store, handle, "HEAD:dir/inner.txt")
		.await?
		.map_err(|error| anyhow!("read-blob post-repack: {error:?}"))?;
	assert_eq!(blob, b"inner\n");
	let listed = porcelain
		.call_ls_tree(&mut *store, handle, "HEAD")
		.await?
		.map_err(|error| anyhow!("ls-tree post-repack: {error:?}"))?;
	assert_eq!(listed.len(), 3);
	let log = porcelain
		.call_rev_list(&mut *store, handle, &["main".to_owned()], None)
		.await?
		.map_err(|error| anyhow!("rev-list post-repack: {error:?}"))?;
	assert_eq!(log.len(), 5);

	// New loose objects + a geometric pass (small pack folds into the big one).
	for i in 0..3u8 {
		porcelain
			.call_write_blob(&mut *store, handle, format!("post-pack {i}\n").as_bytes())
			.await?
			.map_err(|error| anyhow!("write-blob post-repack: {error:?}"))?;
	}
	let geometric = porcelain
		.call_repack(&mut *store, handle, true)
		.await?
		.map_err(|error| anyhow!("geometric repack: {error:?}"))?;
	assert!(geometric.is_some(), "loose objects existed to pack");

	Ok(())
}

#[tokio::test]
async fn write_tree_rejects_malformed_entries() -> Result<()> {
	use gitana_repo_host::exports::gitana::repo::porcelain::{FileMode, TreeBuildEntry};

	let fixture = build_fixture::<Sha256>().await?;
	let mut session = Session::open(fixture.dir.path()).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	let blob = porcelain
		.call_write_blob(&mut *store, handle, b"entry\n")
		.await?
		.map_err(|error| anyhow!("write-blob: {error:?}"))?;
	let entry = |path: &str, id: &str| TreeBuildEntry {
		path: path.to_owned(),
		mode: FileMode::Regular,
		id: id.to_owned(),
	};

	// Traversal-ish and NUL-carrying paths are rejected before anything is written.
	for path in ["", "..", "a/../b", "a//b", ".", "a/\0b"] {
		let bad = porcelain
			.call_write_tree(&mut *store, handle, &[entry(path, &blob)])
			.await?;
		assert!(
			matches!(bad, Err(RepoError::Invalid(_))),
			"{path:?}: {bad:?}"
		);
	}

	// Duplicate paths and file/directory conflicts encode trees fsck rejects.
	let duplicate = porcelain
		.call_write_tree(
			&mut *store,
			handle,
			&[entry("same.txt", &blob), entry("same.txt", &blob)],
		)
		.await?;
	assert!(
		matches!(duplicate, Err(RepoError::Invalid(_))),
		"{duplicate:?}"
	);
	let conflict = porcelain
		.call_write_tree(
			&mut *store,
			handle,
			&[entry("a", &blob), entry("a/b.txt", &blob)],
		)
		.await?;
	assert!(
		matches!(conflict, Err(RepoError::Invalid(_))),
		"{conflict:?}"
	);

	// Dangling and non-blob ids are rejected too.
	let dangling = porcelain
		.call_write_tree(&mut *store, handle, &[entry("ok.txt", &"1".repeat(64))])
		.await?;
	assert!(
		matches!(dangling, Err(RepoError::Invalid(_))),
		"{dangling:?}"
	);
	let not_a_blob = porcelain
		.call_write_tree(&mut *store, handle, &[entry("ok.txt", &fixture.m)])
		.await?;
	assert!(
		matches!(not_a_blob, Err(RepoError::Invalid(_))),
		"{not_a_blob:?}"
	);

	Ok(())
}
