//! End-to-end proof of the descriptor-capability boundary.
//!
//! A native gitana builds a fixture repository; the wasm component — instantiated with
//! **no preopens and no ambient authority** — is handed exactly one directory
//! descriptor and must read a commit and write a blob byte-identical to native gitana,
//! in both hash formats.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use cap_std::ambient_authority;
use gitana_file_store_local::LocalFileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repo_host::exports::gitana::repo::porcelain::HashKind;
use gitana_repo_host::{engine, grant_dir, instantiate, store};
use gitana_repository::{FileMode, Repository, TreeBuildEntry};

const AUTHOR: &str = "A U Thor <author@example.com> 1719900000 +0000";
const COMMITTER: &str = "C O Mitter <committer@example.com> 1719900001 +0000";
const MESSAGE: &str = "initial commit\n";
const NATIVE_BLOB: &[u8] = b"hello from native gitana\n";
const GUEST_BLOB: &[u8] = b"hello from wasm gitana\n";

/// Build the guest component once per test process and return its path.
fn build_component() -> &'static Path {
	static WASM: OnceLock<PathBuf> = OnceLock::new();
	WASM.get_or_init(|| {
		let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
		// crates/wasm/gitana-repo-host → up three levels to the workspace root.
		let workspace = manifest.ancestors().nth(3).expect("workspace root");
		let status = Command::new(env!("CARGO"))
			.current_dir(workspace)
			.args([
				"build",
				"-p",
				"gitana-repo-component",
				"--target",
				"wasm32-wasip2",
			])
			.status()
			.expect("run cargo build for the guest component");
		assert!(status.success(), "guest component build failed");
		// A relative CARGO_TARGET_DIR is resolved by the nested cargo against its cwd —
		// the workspace root above — so anchor it there too.
		let target = std::env::var_os("CARGO_TARGET_DIR")
			.map(|dir| {
				let dir = PathBuf::from(dir);
				if dir.is_absolute() {
					dir
				} else {
					workspace.join(dir)
				}
			})
			.unwrap_or_else(|| workspace.join("target"));
		target.join("wasm32-wasip2/debug/gitana_repo_component.wasm")
	})
}

/// The native-side facts the guest's answers are checked against.
struct Oracle {
	commit: String,
	tree: String,
}

/// Open the fixture directory as a native gitana repository (the test's oracle view).
fn native_repo<H: HashAlgorithm>(git_dir: &Path) -> Result<Repository<LocalFileStore, H>> {
	let dir = cap_std::fs::Dir::open_ambient_dir(git_dir, ambient_authority())?;
	Ok(Repository::new(ObjectStore::new(LocalFileStore::from_dir(
		dir,
	))))
}

/// Create a one-commit repository in `git_dir` natively and record the oracle ids.
async fn build_fixture<H: HashAlgorithm>(git_dir: &Path) -> Result<Oracle> {
	let repo = native_repo::<H>(git_dir)?;
	repo.init().await?;
	let blob = repo.write_blob(NATIVE_BLOB).await?;
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: "hello.txt".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await?;
	let commit = repo
		.create_commit(tree, Vec::new(), AUTHOR, COMMITTER, MESSAGE)
		.await?;
	repo
		.refs()
		.update_ref("refs/heads/main", commit, None)
		.await?;
	Ok(Oracle {
		commit: commit.to_hex(),
		tree: tree.to_hex(),
	})
}

async fn roundtrip<H: HashAlgorithm>(expected_kind: HashKind) -> Result<()> {
	let component = build_component();
	let tmp = tempfile::tempdir()?;
	let git_dir = tmp.path();
	let oracle = build_fixture::<H>(git_dir).await?;

	let engine = engine()?;
	let mut store = store(&engine);
	let repo = instantiate(&engine, &mut store, component).await?;
	// The single capability grant: one directory descriptor, no preopens.
	let dir = grant_dir(&mut store, git_dir)?;

	let porcelain = repo.gitana_repo_porcelain().repository();
	let handle = porcelain
		.call_open(&mut store, dir)
		.await?
		.map_err(|error| anyhow!("open: {error:?}"))?;

	// The guest detected the hash algorithm from config, through the descriptor.
	let kind = porcelain.call_hash_kind(&mut store, handle).await?;
	assert_eq!(kind, expected_kind);

	// Read path: HEAD → symbolic ref → loose ref → commit object, field-for-field.
	let commit = porcelain
		.call_read_commit(&mut store, handle, "HEAD")
		.await?
		.map_err(|error| anyhow!("read-commit: {error:?}"))?;
	assert_eq!(commit.id, oracle.commit);
	assert_eq!(commit.tree, oracle.tree);
	assert!(commit.parents.is_empty());
	assert_eq!(commit.author, AUTHOR);
	assert_eq!(commit.committer, COMMITTER);
	assert_eq!(commit.message, MESSAGE);

	// Write path: the guest's loose object must get the id native gitana computes…
	let expected_id = ObjectId::<H>::compute(ObjectKind::Blob, GUEST_BLOB).to_hex();
	let written = porcelain
		.call_write_blob(&mut store, handle, GUEST_BLOB)
		.await?
		.map_err(|error| anyhow!("write-blob: {error:?}"))?;
	assert_eq!(written, expected_id);

	// …be idempotent on a repeat write…
	let again = porcelain
		.call_write_blob(&mut store, handle, GUEST_BLOB)
		.await?
		.map_err(|error| anyhow!("write-blob (repeat): {error:?}"))?;
	assert_eq!(again, expected_id);

	// …and read back byte-identical through native gitana.
	let native = native_repo::<H>(git_dir)?;
	let bytes = native.read_blob(ObjectId::from_hex(&written)?).await?;
	assert_eq!(bytes, GUEST_BLOB);

	// An unknown revision surfaces as a typed error, not a trap.
	let missing = porcelain
		.call_read_commit(&mut store, handle, "does-not-exist")
		.await?;
	assert!(missing.is_err(), "expected an error for an unknown spec");

	Ok(())
}

#[tokio::test]
async fn sha256_roundtrip_through_descriptor_capability() -> Result<()> {
	roundtrip::<Sha256>(HashKind::Sha256).await
}

#[tokio::test]
async fn sha1_roundtrip_through_descriptor_capability() -> Result<()> {
	roundtrip::<Sha1>(HashKind::Sha1).await
}
