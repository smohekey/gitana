//! End-to-end proof of the descriptor-capability boundary.
//!
//! A native gitana builds a fixture repository; the wasm component — instantiated with
//! **no preopens and no ambient authority** — is handed exactly one directory
//! descriptor and must answer every read, revision, and write byte-identically to
//! native gitana, in both hash formats.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use anyhow::{Result, anyhow, bail};
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, Sha1, Sha256, parse_commit};
use gitana_repo_host::exports::gitana::repo::porcelain::{
	FileMode as WitMode, HashKind, HeadState, ObjectKind as WitKind, RepoError, TreeBuildEntry,
};

use self::support::{AUTHOR, Session, build_fixture, committer, native_repo};

const GUEST_BLOB: &[u8] = b"hello from wasm gitana\n";

async fn roundtrip<H: HashAlgorithm>(expected_kind: HashKind) -> Result<()> {
	let fixture = build_fixture::<H>().await?;
	let git_dir = fixture.dir.path();
	let mut session = Session::open(git_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// The guest detected the hash algorithm from config, through the descriptor.
	assert_eq!(
		porcelain.call_hash_kind(&mut *store, handle).await?,
		expected_kind
	);

	// The raw config text is readable and reflects the format.
	let config = porcelain
		.call_read_config(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("read-config: {error:?}"))?;
	match expected_kind {
		HashKind::Sha256 => assert!(config.contains("objectformat = sha256"), "{config}"),
		HashKind::Sha1 => assert!(!config.contains("objectformat"), "{config}"),
	}

	// -- reads ------------------------------------------------------------------

	let commit = porcelain
		.call_read_commit(&mut *store, handle, "HEAD")
		.await?
		.map_err(|error| anyhow!("read-commit: {error:?}"))?;
	assert_eq!(commit.id, fixture.m);
	assert_eq!(commit.tree, fixture.tree_m);
	assert_eq!(commit.parents, vec![fixture.c.clone(), fixture.d.clone()]);
	assert_eq!(commit.author, AUTHOR);
	assert_eq!(commit.committer, committer(40));
	assert_eq!(commit.message, "merge C and D\n");

	// read-object across all four kinds, byte-equal to the native oracle.
	let native = native_repo::<H>(git_dir)?;
	for (spec, id, kind) in [
		("HEAD:hello.txt", None, WitKind::Blob),
		(
			fixture.tree_m.as_str(),
			Some(&fixture.tree_m),
			WitKind::Tree,
		),
		("main", Some(&fixture.m), WitKind::Commit),
		("annot", Some(&fixture.annot), WitKind::Tag),
	] {
		let object = porcelain
			.call_read_object(&mut *store, handle, spec)
			.await?
			.map_err(|error| anyhow!("read-object {spec}: {error:?}"))?;
		if let Some(id) = id {
			assert_eq!(&object.id, id, "{spec}");
		}
		assert_eq!(object.kind, kind, "{spec}");
		let (native_kind, native_payload) = native
			.objects()
			.read_object(&ObjectId::from_hex(&object.id)?)
			.await?;
		assert_eq!(native_kind.as_str(), kind_name(kind), "{spec}");
		assert_eq!(object.payload, native_payload, "{spec}");
	}

	let inner = porcelain
		.call_read_blob(&mut *store, handle, "HEAD:dir/inner.txt")
		.await?
		.map_err(|error| anyhow!("read-blob: {error:?}"))?;
	assert_eq!(inner, b"inner\n");

	let tag = porcelain
		.call_read_tag(&mut *store, handle, "annot")
		.await?
		.map_err(|error| anyhow!("read-tag: {error:?}"))?;
	assert_eq!(tag.id, fixture.annot);
	assert_eq!(tag.target, fixture.c);
	assert_eq!(tag.target_kind, WitKind::Commit);
	assert_eq!(tag.name, "annot");
	assert!(tag.tagger.is_some());
	assert_eq!(tag.message, "annotated tag\n");

	// ls-tree equals the native recursive read, incl. the executable's mode.
	let listed = porcelain
		.call_ls_tree(&mut *store, handle, "HEAD")
		.await?
		.map_err(|error| anyhow!("ls-tree: {error:?}"))?;
	let native_tree = native
		.read_tree(ObjectId::from_hex(&fixture.tree_m)?)
		.await?;
	assert_eq!(listed.len(), native_tree.len());
	for (entry, (path, mode, id)) in listed.iter().zip(&native_tree) {
		assert_eq!(&entry.path, path);
		assert_eq!(&entry.mode, mode);
		assert_eq!(entry.id, id.to_hex());
	}
	let tool = listed
		.iter()
		.find(|entry| entry.path == "tool")
		.expect("tool entry");
	assert_eq!(tool.mode, "100755");

	// -- revisions --------------------------------------------------------------

	for (spec, expected) in [
		("main", &fixture.m),
		("HEAD~1", &fixture.c),
		(&fixture.m[..8], &fixture.m),
		("annot^{commit}", &fixture.c),
		("lw", &fixture.b),
	] {
		let resolved = porcelain
			.call_rev_parse(&mut *store, handle, spec)
			.await?
			.map_err(|error| anyhow!("rev-parse {spec}: {error:?}"))?;
		assert_eq!(&resolved, expected, "{spec}");
	}

	let log = porcelain
		.call_rev_list(&mut *store, handle, &["main".to_owned()], None)
		.await?
		.map_err(|error| anyhow!("rev-list: {error:?}"))?;
	assert_eq!(
		log,
		vec![
			fixture.m.clone(),
			fixture.c.clone(),
			fixture.d.clone(),
			fixture.b.clone(),
			fixture.a.clone()
		]
	);
	let truncated = porcelain
		.call_rev_list(&mut *store, handle, &["main".to_owned()], Some(2))
		.await?
		.map_err(|error| anyhow!("rev-list max-count: {error:?}"))?;
	assert_eq!(truncated, vec![fixture.m.clone(), fixture.c.clone()]);

	let base = porcelain
		.call_merge_base(&mut *store, handle, &[fixture.c.clone(), fixture.d.clone()])
		.await?
		.map_err(|error| anyhow!("merge-base: {error:?}"))?;
	assert_eq!(base, vec![fixture.b.clone()]);
	let none = porcelain
		.call_merge_base(
			&mut *store,
			handle,
			&["main".to_owned(), "orphan".to_owned()],
		)
		.await?
		.map_err(|error| anyhow!("merge-base orphan: {error:?}"))?;
	assert!(none.is_empty(), "unrelated roots share no merge base");

	assert!(
		porcelain
			.call_is_ancestor(&mut *store, handle, &fixture.a, "main")
			.await?
			.map_err(|error| anyhow!("is-ancestor: {error:?}"))?
	);
	assert!(
		!porcelain
			.call_is_ancestor(&mut *store, handle, &fixture.c, &fixture.d)
			.await?
			.map_err(|error| anyhow!("is-ancestor siblings: {error:?}"))?
	);

	// -- refs -------------------------------------------------------------------

	let heads = porcelain
		.call_list_refs(&mut *store, handle, "refs/heads/")
		.await?
		.map_err(|error| anyhow!("list-refs: {error:?}"))?;
	let named: Vec<(&str, &str)> = heads
		.iter()
		.map(|entry| (entry.name.as_str(), entry.id.as_str()))
		.collect();
	assert_eq!(
		named,
		vec![
			("refs/heads/feature", fixture.d.as_str()),
			// The loose `main → M` wins over the stale packed `main → A`.
			("refs/heads/main", fixture.m.as_str()),
			("refs/heads/orphan", fixture.orphan.as_str()),
			// Present only in the hand-written packed-refs.
			("refs/heads/packed", fixture.a.as_str()),
		]
	);

	let head = porcelain
		.call_head(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("head: {error:?}"))?;
	match &head {
		HeadState::Symbolic(symbolic) => {
			assert_eq!(symbolic.target, "refs/heads/main");
			assert_eq!(symbolic.id, fixture.m);
		}
		other => bail!("expected symbolic HEAD, got {other:?}"),
	}

	let hit = porcelain
		.call_resolve_ref(&mut *store, handle, "refs/tags/lw")
		.await?
		.map_err(|error| anyhow!("resolve-ref: {error:?}"))?;
	assert_eq!(hit, Some(fixture.b.clone()));
	let miss = porcelain
		.call_resolve_ref(&mut *store, handle, "refs/heads/nope")
		.await?
		.map_err(|error| anyhow!("resolve-ref miss: {error:?}"))?;
	assert_eq!(miss, None);

	let symref = porcelain
		.call_read_symbolic_ref(&mut *store, handle, "HEAD")
		.await?
		.map_err(|error| anyhow!("read-symbolic-ref: {error:?}"))?;
	assert_eq!(symref.as_deref(), Some("refs/heads/main"));

	// -- writes -----------------------------------------------------------------

	let expected_id = ObjectId::<H>::compute(ObjectKind::Blob, GUEST_BLOB).to_hex();
	let written = porcelain
		.call_write_blob(&mut *store, handle, GUEST_BLOB)
		.await?
		.map_err(|error| anyhow!("write-blob: {error:?}"))?;
	assert_eq!(written, expected_id);
	let again = porcelain
		.call_write_blob(&mut *store, handle, GUEST_BLOB)
		.await?
		.map_err(|error| anyhow!("write-blob (repeat): {error:?}"))?;
	assert_eq!(again, expected_id);
	let bytes = native.read_blob(ObjectId::from_hex(&written)?).await?;
	assert_eq!(bytes, GUEST_BLOB);

	// A guest-built tree + commit + ref, verified natively end-to-end.
	let entries = vec![TreeBuildEntry {
		path: "guest/data.txt".to_owned(),
		mode: WitMode::Regular,
		id: written.clone(),
	}];
	let guest_tree = porcelain
		.call_write_tree(&mut *store, handle, &entries)
		.await?
		.map_err(|error| anyhow!("write-tree: {error:?}"))?;
	let guest_commit = porcelain
		.call_create_commit(
			&mut *store,
			handle,
			&guest_tree,
			&[fixture.m.clone()],
			AUTHOR,
			&committer(60),
			"guest commit\n",
		)
		.await?
		.map_err(|error| anyhow!("create-commit: {error:?}"))?;
	// Identical inputs produce the identical commit id.
	let same = porcelain
		.call_create_commit(
			&mut *store,
			handle,
			&guest_tree,
			&[fixture.m.clone()],
			AUTHOR,
			&committer(60),
			"guest commit\n",
		)
		.await?
		.map_err(|error| anyhow!("create-commit (repeat): {error:?}"))?;
	assert_eq!(same, guest_commit);
	porcelain
		.call_update_ref(&mut *store, handle, "refs/heads/guest", &guest_commit, None)
		.await?
		.map_err(|error| anyhow!("update-ref: {error:?}"))?;

	let (kind, payload) = native
		.objects()
		.read_object(&ObjectId::from_hex(&guest_commit)?)
		.await?;
	assert_eq!(kind, ObjectKind::Commit);
	let parsed = parse_commit::<H>(&payload)?;
	assert_eq!(parsed.tree.to_hex(), guest_tree);
	assert_eq!(parsed.parents.len(), 1);
	assert_eq!(parsed.parents[0].to_hex(), fixture.m);
	assert_eq!(parsed.message, "guest commit\n");
	let guest_entries = native.read_tree(ObjectId::from_hex(&guest_tree)?).await?;
	assert_eq!(guest_entries.len(), 1);
	assert_eq!(guest_entries[0].0, "guest/data.txt");
	assert_eq!(guest_entries[0].1, "100644");
	assert_eq!(guest_entries[0].2.to_hex(), written);
	assert_eq!(
		native.refs().resolve("refs/heads/guest").await?,
		Some(ObjectId::from_hex(&guest_commit)?)
	);

	// A commit whose tree spec is not a tree is invalid input.
	let bad = porcelain
		.call_create_commit(
			&mut *store,
			handle,
			"main",
			&[],
			AUTHOR,
			&committer(70),
			"bad\n",
		)
		.await?;
	assert!(matches!(bad, Err(RepoError::Invalid(_))), "{bad:?}");

	// -- typed errors -------------------------------------------------------------

	let missing = porcelain
		.call_read_commit(&mut *store, handle, "does-not-exist")
		.await?;
	assert!(
		matches!(missing, Err(RepoError::UnknownRevision(_))),
		"expected unknown-revision, got {missing:?}"
	);
	let not_a_commit = porcelain
		.call_read_commit(&mut *store, handle, "HEAD^{tree}")
		.await?;
	assert!(
		matches!(not_a_commit, Err(RepoError::Invalid(_))),
		"expected invalid for a tree spec, got {not_a_commit:?}"
	);

	Ok(())
}

fn kind_name(kind: WitKind) -> &'static str {
	match kind {
		WitKind::Blob => "blob",
		WitKind::Tree => "tree",
		WitKind::Commit => "commit",
		WitKind::Tag => "tag",
	}
}

#[tokio::test]
async fn sha256_roundtrip_through_descriptor_capability() -> Result<()> {
	roundtrip::<Sha256>(HashKind::Sha256).await
}

#[tokio::test]
async fn sha1_roundtrip_through_descriptor_capability() -> Result<()> {
	roundtrip::<Sha1>(HashKind::Sha1).await
}
