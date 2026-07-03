//! Shared harness for the host integration tests: guest build, native fixture
//! construction, and a wasmtime session helper.

#![allow(dead_code)] // each test binary uses a subset of these helpers

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use cap_std::ambient_authority;
use gitana_file_store_local::LocalFileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, Tag, encode_tag};
use gitana_object_store::ObjectStore;
use gitana_repo_host::exports::gitana::repo::porcelain::{HashKind, RepoError};
use gitana_repo_host::{Repo, State, engine, grant_dir, instantiate, store};
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use wasmtime::Store;
use wasmtime::component::ResourceAny;

pub const AUTHOR: &str = "A U Thor <author@example.com> 1719900000 +0000";

/// A committer identity line `offset` seconds after the fixture epoch — rev-list
/// ordering in the fixture is driven entirely by these.
pub fn committer(offset: u64) -> String {
	format!(
		"C O Mitter <committer@example.com> {} +0000",
		1_719_900_000 + offset
	)
}

/// Build the guest component once per test process and return its path.
pub fn build_component() -> &'static Path {
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

/// Open the fixture directory as a native gitana repository (the tests' oracle view).
pub fn native_repo<H: HashAlgorithm>(git_dir: &Path) -> Result<Repository<LocalFileStore, H>> {
	let dir = cap_std::fs::Dir::open_ambient_dir(git_dir, ambient_authority())?;
	Ok(Repository::new(ObjectStore::new(LocalFileStore::from_dir(
		dir,
	))))
}

/// The natively-built fixture the guest's answers are checked against.
///
/// History (committer dates strictly increasing A→M; ids in hex):
///
/// ```text
///   A ── B ── C ──── M   (refs/heads/main)
///         \         /
///          ── D ────     (refs/heads/feature)
/// ```
///
/// plus an unrelated root `orphan`, a lightweight tag `lw → B`, an annotated tag
/// object `annot → C`, and a hand-written `packed-refs` holding `refs/heads/packed → A`
/// and a stale `refs/heads/main → A` shadowed by the loose `main → M`.
pub struct Fixture {
	pub dir: tempfile::TempDir,
	pub a: String,
	pub b: String,
	pub c: String,
	pub d: String,
	pub m: String,
	pub orphan: String,
	pub tree_m: String,
	pub annot: String,
}

/// Create the fixture repository natively.
pub async fn build_fixture<H: HashAlgorithm>() -> Result<Fixture> {
	let dir = tempfile::tempdir()?;
	let repo = native_repo::<H>(dir.path())?;
	repo.init().await?;

	let hello1 = repo.write_blob(b"hello v1\n").await?;
	let hello3 = repo.write_blob(b"hello v3\n").await?;
	let inner = repo.write_blob(b"inner\n").await?;
	let tool = repo.write_blob(b"#!/bin/sh\nexit 0\n").await?;

	let entry = |path: &str, mode: FileMode, id: ObjectId<H>| TreeBuildEntry {
		path: path.to_owned(),
		mode,
		id,
	};
	let tree_a = repo
		.write_tree(&[entry("hello.txt", FileMode::Regular, hello1)])
		.await?;
	let tree_b = repo
		.write_tree(&[
			entry("dir/inner.txt", FileMode::Regular, inner),
			entry("hello.txt", FileMode::Regular, hello1),
		])
		.await?;
	let tree_c = repo
		.write_tree(&[
			entry("dir/inner.txt", FileMode::Regular, inner),
			entry("hello.txt", FileMode::Regular, hello3),
		])
		.await?;
	let tree_d = repo
		.write_tree(&[
			entry("dir/inner.txt", FileMode::Regular, inner),
			entry("hello.txt", FileMode::Regular, hello1),
			entry("tool", FileMode::Executable, tool),
		])
		.await?;
	let tree_m = repo
		.write_tree(&[
			entry("dir/inner.txt", FileMode::Regular, inner),
			entry("hello.txt", FileMode::Regular, hello3),
			entry("tool", FileMode::Executable, tool),
		])
		.await?;

	let a = repo
		.create_commit(tree_a, Vec::new(), AUTHOR, &committer(0), "A\n")
		.await?;
	let b = repo
		.create_commit(tree_b, vec![a], AUTHOR, &committer(10), "B\n")
		.await?;
	let d = repo
		.create_commit(tree_d, vec![b], AUTHOR, &committer(20), "D\n")
		.await?;
	let c = repo
		.create_commit(tree_c, vec![b], AUTHOR, &committer(30), "C\n")
		.await?;
	let m = repo
		.create_commit(
			tree_m,
			vec![c, d],
			AUTHOR,
			&committer(40),
			"merge C and D\n",
		)
		.await?;
	let orphan = repo
		.create_commit(tree_a, Vec::new(), AUTHOR, &committer(50), "orphan\n")
		.await?;

	repo.refs().update_ref("refs/heads/main", m, None).await?;
	repo
		.refs()
		.update_ref("refs/heads/feature", d, None)
		.await?;
	repo
		.refs()
		.update_ref("refs/heads/orphan", orphan, None)
		.await?;
	repo.refs().update_ref("refs/tags/lw", b, None).await?;

	let tag = Tag {
		object: c,
		kind: ObjectKind::Commit,
		name: "annot".to_owned(),
		tagger: Some("T A Gger <tagger@example.com> 1719900100 +0000".to_owned()),
		message: "annotated tag\n".to_owned(),
	};
	let annot = repo
		.objects()
		.write_object(ObjectKind::Tag, &encode_tag(&tag))
		.await?;
	repo
		.refs()
		.update_ref("refs/tags/annot", annot, None)
		.await?;

	// No packed-refs writer exists in gitana; hand-write the file (header + `oid name`
	// lines). A stale `main` entry proves loose-wins merging in `list-refs`.
	std::fs::write(
		dir.path().join("packed-refs"),
		format!(
			"# pack-refs with: peeled fully-peeled sorted\n{a} refs/heads/main\n{a} refs/heads/packed\n",
			a = a.to_hex()
		),
	)?;

	Ok(Fixture {
		dir,
		a: a.to_hex(),
		b: b.to_hex(),
		c: c.to_hex(),
		d: d.to_hex(),
		m: m.to_hex(),
		orphan: orphan.to_hex(),
		tree_m: tree_m.to_hex(),
		annot: annot.to_hex(),
	})
}

/// An instantiated component with an opened repository resource.
pub struct Session {
	pub store: Store<State>,
	pub repo: Repo,
	pub handle: ResourceAny,
}

impl Session {
	/// Instantiate the guest (no preopens), grant `git_dir` as a descriptor, and
	/// open the repository through it.
	pub async fn open(git_dir: &Path) -> Result<Self> {
		let engine = engine()?;
		let mut store = store(&engine);
		let repo = instantiate(&engine, &mut store, build_component()).await?;
		let dir = grant_dir(&mut store, git_dir)?;
		let handle = repo
			.gitana_repo_porcelain()
			.repository()
			.call_open(&mut store, dir)
			.await?
			.map_err(|error| anyhow!("open: {error:?}"))?;
		Ok(Self {
			store,
			repo,
			handle,
		})
	}

	/// Like [`Session::open`], but through the guest's `init` export; the guest's
	/// typed error is preserved for tests asserting init failures.
	pub async fn try_init(git_dir: &Path, kind: HashKind) -> Result<Result<Self, RepoError>> {
		let engine = engine()?;
		let mut store = store(&engine);
		let repo = instantiate(&engine, &mut store, build_component()).await?;
		let dir = grant_dir(&mut store, git_dir)?;
		let opened = repo
			.gitana_repo_porcelain()
			.repository()
			.call_init(&mut store, dir, kind)
			.await?;
		Ok(opened.map(|handle| Self {
			store,
			repo,
			handle,
		}))
	}

	/// Initialize a fresh repository via the guest and open it.
	pub async fn init(git_dir: &Path, kind: HashKind) -> Result<Self> {
		Self::try_init(git_dir, kind)
			.await?
			.map_err(|error| anyhow!("init: {error:?}"))
	}
}
