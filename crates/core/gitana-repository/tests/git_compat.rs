// Cross-checked against real `git` and built on the native cap-std store (`from_dir`), so
// this suite is native-only.
#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_file_store::FileStore;
use gitana_file_store_local::LocalFileStore;
use gitana_file_store_memory::MemoryFileStore;
use gitana_object::{ObjectId, ObjectKind, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::{
	FileMode, HeadState, ReflogIntent, Repository, TreeBuildEntry, compute_tree_id,
};

fn open_dir(path: impl AsRef<std::path::Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn mem_repo() -> Repository<MemoryFileStore, Sha256> {
	Repository::new(ObjectStore::new(MemoryFileStore::new()))
}

#[tokio::test]
async fn init_open_and_loose_ref_cas() {
	let repo = mem_repo();
	repo.init().await.unwrap();
	assert_eq!(repo.open().await.unwrap().object_format, "sha256");

	let refs = repo.refs();
	assert_eq!(
		refs.read_head().await.unwrap(),
		HeadState::Symbolic("refs/heads/main".to_owned())
	);
	// Unborn branch: HEAD resolves to nothing yet.
	assert_eq!(refs.resolve_head().await.unwrap(), None);

	let first = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"c1");
	refs
		.update_ref("refs/heads/main", first, None, ReflogIntent::Skip)
		.await
		.unwrap();
	assert_eq!(refs.resolve_head().await.unwrap(), Some(first));

	// CAS-create on an existing ref fails; CAS-update with the right expected works.
	let second = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"c2");
	assert!(
		refs
			.update_ref("refs/heads/main", second, None, ReflogIntent::Skip)
			.await
			.is_err()
	);
	refs
		.update_ref("refs/heads/main", second, Some(first), ReflogIntent::Skip)
		.await
		.unwrap();
	assert_eq!(refs.resolve("refs/heads/main").await.unwrap(), Some(second));

	// Stale expected fails.
	assert!(
		refs
			.update_ref("refs/heads/main", first, Some(first), ReflogIntent::Skip)
			.await
			.is_err()
	);
}

#[tokio::test]
async fn rev_parse_dwims_remote_tracking_refs() {
	// git's gitrevisions(7) search order resolves `origin/main` to `refs/remotes/origin/main`, and a
	// bare remote name (or `origin/HEAD`) through the remote's symbolic `refs/remotes/origin/HEAD`.
	let repo = mem_repo();
	repo.init().await.unwrap();
	let refs = repo.refs();

	let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"remote-tip");
	refs
		.update_ref("refs/remotes/origin/main", tip, None, ReflogIntent::Skip)
		.await
		.unwrap();
	// The remote's HEAD is a symbolic ref pointing at its default branch.
	refs
		.set_symbolic(
			"refs/remotes/origin/HEAD",
			"refs/remotes/origin/main",
			ReflogIntent::Skip,
		)
		.await
		.unwrap();

	assert_eq!(repo.rev_parse("origin/main").await.unwrap(), tip);
	// The fully-qualified form resolves too.
	assert_eq!(
		repo.rev_parse("refs/remotes/origin/main").await.unwrap(),
		tip
	);
	// A bare remote name resolves through refs/remotes/<name>/HEAD, following the symbolic ref.
	assert_eq!(repo.rev_parse("origin").await.unwrap(), tip);
	// `origin/HEAD` and its fully-qualified form both name that symbolic ref directly.
	assert_eq!(repo.rev_parse("origin/HEAD").await.unwrap(), tip);
	assert_eq!(
		repo.rev_parse("refs/remotes/origin/HEAD").await.unwrap(),
		tip
	);
}

#[tokio::test]
async fn rev_parse_treats_a_ref_directory_as_a_miss() {
	// On a real (directory-backed) store, a bare name whose only match is a directory —
	// `refs/remotes/origin` (holding `origin/main`), or a hierarchical branch namespace —
	// must resolve to a clean UnknownRevision, not a backend directory-read error.
	let work = unique_tmp("revdir");
	let git_dir = work.join(".git");
	create_skeleton(&git_dir);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();
	let refs = repo.refs();

	let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"remote-tip");
	refs
		.update_ref("refs/remotes/origin/main", tip, None, ReflogIntent::Skip)
		.await
		.unwrap();
	// A hierarchical local branch makes `refs/heads/feature` a directory.
	refs
		.update_ref("refs/heads/feature/x", tip, None, ReflogIntent::Skip)
		.await
		.unwrap();

	// The leaf remote-tracking branch still resolves.
	assert_eq!(repo.rev_parse("origin/main").await.unwrap(), tip);
	// Bare names that only name a ref *directory* miss cleanly rather than erroring.
	assert!(matches!(
		repo.rev_parse("origin").await,
		Err(gitana_repository::RepositoryError::UnknownRevision(_))
	));
	assert!(matches!(
		repo.rev_parse("feature").await,
		Err(gitana_repository::RepositoryError::UnknownRevision(_))
	));
}

#[tokio::test]
async fn abbreviations_resolve_across_loose_and_packed_objects() {
	let repo = mem_repo();
	repo.init().await.unwrap();

	// A few commits, so several ids exist as loose objects.
	let mut tips = Vec::new();
	for i in 0..3u64 {
		let blob = repo
			.write_blob(format!("content {i}\n").as_bytes())
			.await
			.unwrap();
		let tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "f.txt".to_owned(),
				mode: FileMode::Regular,
				id: blob,
			}])
			.await
			.unwrap();
		let sig = format!("T E St <t@e> {} +0000", 1_700_000_000 + i as i64);
		tips.push(
			repo
				.commit_on_head(tree, &sig, &sig, &format!("c{i}\n"))
				.await
				.unwrap(),
		);
	}
	let target = tips[0];
	let abbrev = &target.to_hex()[..12];

	// Loose: the abbreviation resolves.
	assert_eq!(repo.rev_parse(abbrev).await.unwrap(), target);

	// Consolidate into a single pack, removing the loose objects.
	repo.objects().repack(u64::MAX).await.unwrap();

	// Packed: the same abbreviation still resolves (the loose-only gap this closes).
	assert_eq!(repo.rev_parse(abbrev).await.unwrap(), target);
}

#[tokio::test]
async fn open_refuses_non_sha256() {
	let repo = mem_repo();
	repo
		.objects()
		.file_store()
		.write_path_if_absent("config", b"[core]\n\trepositoryformatversion = 0\n")
		.await
		.unwrap();
	assert!(repo.open().await.is_err());
}

#[tokio::test]
async fn git_accepts_an_engine_initialised_repo() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("repo-init");
	let git_dir = work.join(".git");
	create_skeleton(&git_dir);

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();

	let work = work.to_str().unwrap();
	assert_eq!(
		git(&["-C", work, "symbolic-ref", "HEAD"]).trim(),
		"refs/heads/main"
	);
	assert_eq!(
		git(&["-C", work, "rev-parse", "--is-inside-work-tree"]).trim(),
		"true"
	);
	assert_eq!(
		git(&["-C", work, "config", "extensions.objectformat"]).trim(),
		"sha256"
	);

	std::fs::remove_dir_all(work).ok();
}

#[tokio::test]
async fn engine_commit_is_read_by_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("commit");
	let git_dir = work.join(".git");
	create_skeleton(&git_dir);

	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();

	let blob = repo.write_blob(b"hello\n").await.unwrap();
	let entries = [
		TreeBuildEntry {
			path: "greeting.txt".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		},
		TreeBuildEntry {
			path: "src/lib.rs".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		},
	];
	let computed_tree = compute_tree_id(&entries).unwrap();
	let tree = repo.write_tree(&entries).await.unwrap();
	assert_eq!(computed_tree, tree);
	let author = "A U Thor <author@example.com> 1700000000 +0000";
	let committer = "C O Mitter <committer@example.com> 1700000000 +0000";
	let commit = repo
		.commit_on_head(tree, author, committer, "first commit\n")
		.await
		.unwrap();

	let work = work.to_str().unwrap();
	// git agrees on HEAD and the tree it points at.
	assert_eq!(
		git(&["-C", work, "rev-parse", "HEAD"]).trim(),
		commit.to_hex()
	);
	assert_eq!(
		git(&["-C", work, "rev-parse", "HEAD^{tree}"]).trim(),
		tree.to_hex()
	);
	// git reads the nested blob content.
	assert_eq!(
		git(&[
			"-C",
			work,
			"cat-file",
			"-p",
			&format!("{}:src/lib.rs", commit.to_hex())
		]),
		"hello\n"
	);
	// git log and reflog see the commit.
	assert!(git(&["-C", work, "log", "--format=%s"]).contains("first commit"));
	assert!(git(&["-C", work, "reflog"]).contains("commit (initial)"));
	// git fsck finds no corruption in the engine-written objects.
	git(&["-C", work, "fsck", "--no-dangling"]);

	std::fs::remove_dir_all(work).ok();
}

#[tokio::test]
async fn git_reads_a_sha1_engine_initialised_repo_and_commit() {
	// sha1 is git's default object format, so this needs only a plain `git`.
	if Command::new("git").arg("--version").output().is_err() {
		eprintln!("skipping: no git on PATH");
		return;
	}
	let work = unique_tmp("sha1-commit");
	let git_dir = work.join(".git");
	create_skeleton(&git_dir);

	let repo = Repository::new(ObjectStore::<_, Sha1>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();

	let blob = repo.write_blob(b"hello\n").await.unwrap();
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: "greeting.txt".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();
	let author = "A U Thor <author@example.com> 1700000000 +0000";
	let committer = "C O Mitter <committer@example.com> 1700000000 +0000";
	let commit = repo
		.commit_on_head(tree, author, committer, "first commit\n")
		.await
		.unwrap();

	let work = work.to_str().unwrap();
	// git sees a classic sha1 repo (version 0) and agrees on the 40-hex commit id.
	assert_eq!(
		git(&["-C", work, "config", "core.repositoryformatversion"]).trim(),
		"0"
	);
	assert_eq!(commit.to_hex().len(), 40);
	assert_eq!(
		git(&["-C", work, "rev-parse", "HEAD"]).trim(),
		commit.to_hex()
	);
	assert_eq!(
		git(&[
			"-C",
			work,
			"cat-file",
			"-p",
			&format!("{}:greeting.txt", commit.to_hex())
		]),
		"hello\n"
	);
	// git fsck finds no corruption in the engine-written sha1 objects.
	git(&["-C", work, "fsck", "--no-dangling"]);

	std::fs::remove_dir_all(work).ok();
}

async fn make_commit(
	repo: &Repository<LocalFileStore, Sha256>,
	content: &[u8],
	secs: i64,
) -> ObjectId<Sha256> {
	let blob = repo.write_blob(content).await.unwrap();
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: "f.txt".to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();
	let sig = format!("T E St <t@e> {secs} +0000");
	repo
		.commit_on_head(tree, &sig, &sig, &format!("c{secs}\n"))
		.await
		.unwrap()
}

#[tokio::test]
async fn revisions_and_packed_refs_match_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("rev");
	let git_dir = work.join(".git");
	create_skeleton(&git_dir);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();

	let c1 = make_commit(&repo, b"a\n", 1_700_000_000).await;
	let c2 = make_commit(&repo, b"b\n", 1_700_000_100).await;
	let c3 = make_commit(&repo, b"c\n", 1_700_000_200).await;
	let work = work.to_str().unwrap();

	// rev-parse: refs, ancestry, peel, abbreviation.
	assert_eq!(repo.rev_parse("HEAD").await.unwrap(), c3);
	assert_eq!(repo.rev_parse("main").await.unwrap(), c3);
	assert_eq!(repo.rev_parse("HEAD~1").await.unwrap(), c2);
	assert_eq!(repo.rev_parse("HEAD^").await.unwrap(), c2);
	assert_eq!(repo.rev_parse("HEAD~2").await.unwrap(), c1);
	assert_eq!(repo.rev_parse(&c1.to_hex()[..12]).await.unwrap(), c1);
	let tree3 = repo.rev_parse("HEAD^{tree}").await.unwrap();

	// git agrees on the same specs.
	assert_eq!(
		git(&["-C", work, "rev-parse", "HEAD~2"]).trim(),
		c1.to_hex()
	);
	assert_eq!(
		git(&["-C", work, "rev-parse", "HEAD^{tree}"]).trim(),
		tree3.to_hex()
	);

	// rev-list order matches git (newest first).
	assert_eq!(repo.rev_list(&[c3]).await.unwrap(), vec![c3, c2, c1]);
	let git_list: Vec<String> = git(&["-C", work, "rev-list", "HEAD"])
		.lines()
		.map(str::to_owned)
		.collect();
	assert_eq!(git_list, vec![c3.to_hex(), c2.to_hex(), c1.to_hex()]);

	// After git packs the refs (loose refs removed), the engine still resolves them.
	git(&["-C", work, "pack-refs", "--all"]);
	assert_eq!(
		repo.refs().resolve("refs/heads/main").await.unwrap(),
		Some(c3)
	);
	assert_eq!(repo.rev_parse("main").await.unwrap(), c3);

	// Deleting a packed-only ref rewrites packed-refs; the engine and git both see it gone.
	repo
		.refs()
		.delete_ref("refs/heads/main", Some(c3), ReflogIntent::Skip)
		.await
		.unwrap();
	assert_eq!(repo.refs().resolve("refs/heads/main").await.unwrap(), None);
	let heads = git(&[
		"-C",
		work,
		"for-each-ref",
		"--format=%(refname)",
		"refs/heads/",
	]);
	assert!(
		!heads.contains("refs/heads/main"),
		"git sees the packed ref removed: {heads:?}"
	);

	std::fs::remove_dir_all(work).ok();
}

fn create_skeleton(git_dir: &Path) {
	let _ = std::fs::remove_dir_all(git_dir);
	for sub in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		std::fs::create_dir_all(git_dir.join(sub)).unwrap();
	}
}

fn unique_tmp(tag: &str) -> PathBuf {
	// A per-call sequence number keeps every temp dir distinct even for a reused tag, so tests running
	// in parallel threads never race on `remove_dir_all`/`create_dir_all` for the same path (which
	// surfaced as a transient `File exists`). Matches the `git_status`/`git_diff`/`git_submodule` harnesses.
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gitana-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git(args: &[&str]) -> String {
	let out = Command::new("git").args(args).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn git_supports_sha256() -> bool {
	let probe = unique_tmp("git-probe");
	let ok = Command::new("git")
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}
