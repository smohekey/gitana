//! End-to-end proof of the two-descriptor linked-worktree open.
//!
//! A native gitana builds the shared repository (the *common* dir). A second directory
//! plays the *linked worktree*'s private git dir, holding only its own `HEAD`. The
//! component — instantiated with **no preopens** — is granted both directories as
//! descriptors and must route each path to the one that owns it: per-worktree files
//! (`HEAD`) to the worktree dir, everything shared (objects, refs, `config`) to the
//! common dir, in both hash formats.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use anyhow::{Result, anyhow, bail};
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, Sha1, Sha256};
use gitana_repo_host::exports::gitana::repo::porcelain::HeadState;

use self::support::{Session, build_fixture, native_repo};

const WT_BLOB: &[u8] = b"worktree blob\n";

async fn routes_across_two_descriptors<H: HashAlgorithm>() -> Result<()> {
	let fixture = build_fixture::<H>().await?;
	let common_dir = fixture.dir.path();

	// The linked worktree's private git dir: its own `HEAD` points at `feature`, distinct from
	// the shared `HEAD` (`main`) that init wrote into the common dir — so which directory a read
	// or write resolves to is provable from the answer.
	let worktree = tempfile::tempdir()?;
	let wt_dir = worktree.path();
	std::fs::write(wt_dir.join("HEAD"), "ref: refs/heads/feature\n")?;

	// This test exercises only the git-dir/common-dir routing, not the working tree, so the
	// work-dir descriptor is a throwaway empty directory.
	let work = tempfile::tempdir()?;
	let mut session = Session::open_worktree(wt_dir, common_dir, work.path()).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// -- per-worktree read: HEAD comes from the worktree dir (→ feature/D), not the common dir's
	//    main/M. The `feature` ref it names is itself resolved from the common dir.
	let head = porcelain
		.call_head(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("head: {error:?}"))?;
	match &head {
		HeadState::Symbolic(symbolic) => {
			assert_eq!(symbolic.target, "refs/heads/feature");
			assert_eq!(symbolic.id, fixture.d);
		}
		other => bail!("expected symbolic HEAD → feature, got {other:?}"),
	}

	// -- shared read: refs resolve from the common dir.
	let main = porcelain
		.call_resolve_ref(&mut *store, handle, "refs/heads/main")
		.await?
		.map_err(|error| anyhow!("resolve-ref: {error:?}"))?;
	assert_eq!(main, Some(fixture.m.clone()));

	// -- per-worktree write: moving HEAD lands in the worktree dir and leaves the common dir's
	//    HEAD untouched.
	porcelain
		.call_set_symbolic_ref(&mut *store, handle, "HEAD", "refs/heads/orphan")
		.await?
		.map_err(|error| anyhow!("set-symbolic-ref: {error:?}"))?;
	assert_eq!(
		std::fs::read_to_string(wt_dir.join("HEAD"))?,
		"ref: refs/heads/orphan\n"
	);
	assert_eq!(
		std::fs::read_to_string(common_dir.join("HEAD"))?,
		"ref: refs/heads/main\n"
	);

	// -- shared write: a new branch under refs/heads/ lands in the common dir, never the worktree.
	porcelain
		.call_update_ref(&mut *store, handle, "refs/heads/wt-new", &fixture.d, None)
		.await?
		.map_err(|error| anyhow!("update-ref: {error:?}"))?;
	assert_eq!(
		native_repo::<H>(common_dir)?
			.refs()
			.resolve("refs/heads/wt-new")
			.await?,
		Some(ObjectId::from_hex(&fixture.d)?)
	);
	assert!(
		!wt_dir.join("refs/heads/wt-new").exists(),
		"a shared ref must not land in the worktree dir"
	);

	// -- object write: objects are shared, so a guest-written blob lands in the common object store.
	let expected = ObjectId::<H>::compute(ObjectKind::Blob, WT_BLOB).to_hex();
	let written = porcelain
		.call_write_blob(&mut *store, handle, WT_BLOB)
		.await?
		.map_err(|error| anyhow!("write-blob: {error:?}"))?;
	assert_eq!(written, expected);
	assert_eq!(
		native_repo::<H>(common_dir)?
			.read_blob(ObjectId::from_hex(&written)?)
			.await?,
		WT_BLOB
	);

	Ok(())
}

#[tokio::test]
async fn sha256_worktree_routing_through_two_descriptors() -> Result<()> {
	routes_across_two_descriptors::<Sha256>().await
}

#[tokio::test]
async fn sha1_worktree_routing_through_two_descriptors() -> Result<()> {
	routes_across_two_descriptors::<Sha1>().await
}
