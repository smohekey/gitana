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
use gitana_path::GitPath;
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
async fn rev_parse_preserves_non_utf8_tree_path_suffixes() {
	let repo = mem_repo();
	repo.init().await.unwrap();
	let blob = repo.write_blob(b"raw path\n").await.unwrap();
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: GitPath::from_bytes(b"bad-\xff".to_vec()).unwrap(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();
	repo
		.commit_on_head(
			tree,
			"T E St <t@e> 1700000000 +0000",
			"T E St <t@e> 1700000000 +0000",
			"raw path\n",
		)
		.await
		.unwrap();

	assert_eq!(repo.rev_parse(b"HEAD:bad-\xff").await.unwrap(), blob);
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
async fn packed_ref_directory_file_conflicts_match_git() {
	for (case, existing, requested) in [
		("packed-child", "refs/tags/topic/child", "refs/tags/topic"),
		("packed-parent", "refs/tags/topic", "refs/tags/topic/child"),
	] {
		let work = unique_tmp(case);
		let bare = work.join("repo.git");
		let payload = work.join("payload");
		std::fs::write(&payload, b"ref namespace probe").unwrap();
		git(&["init", "--bare", bare.to_str().unwrap()]);
		let oid = git(&[
			"--git-dir",
			bare.to_str().unwrap(),
			"hash-object",
			"-w",
			payload.to_str().unwrap(),
		]);
		let oid = oid.trim();
		git(&[
			"--git-dir",
			bare.to_str().unwrap(),
			"update-ref",
			existing,
			oid,
		]);
		git(&[
			"--git-dir",
			bare.to_str().unwrap(),
			"pack-refs",
			"--all",
			"--prune",
		]);
		let output = Command::new("git")
			.args([
				"--git-dir",
				bare.to_str().unwrap(),
				"update-ref",
				requested,
				oid,
			])
			.output()
			.expect("run git update-ref");
		assert!(
			!output.status.success(),
			"git must reject {requested:?} against packed {existing:?}"
		);

		let files = MemoryFileStore::new();
		files
			.write_path_if_absent("packed-refs", format!("{oid} {existing}\n").as_bytes())
			.await
			.unwrap();
		let refs = gitana_repository::RefStore::<_, Sha1>::new(&files);
		let object = ObjectId::<Sha1>::from_hex(oid).unwrap();
		assert!(
			refs
				.update_ref(requested, object, None, ReflogIntent::Skip)
				.await
				.is_err(),
			"gitana must reject the same packed namespace conflict"
		);

		std::fs::remove_dir_all(work).ok();
	}
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
				path: GitPath::from_utf8("f.txt").unwrap(),
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
			path: GitPath::from_utf8("greeting.txt").unwrap(),
			mode: FileMode::Regular,
			id: blob,
		},
		TreeBuildEntry {
			path: GitPath::from_utf8("src/lib.rs").unwrap(),
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
			path: GitPath::from_utf8("greeting.txt").unwrap(),
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
			path: GitPath::from_utf8("f.txt").unwrap(),
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

/// `git` in `dir` with its author and committer dated `secs` seconds past the epoch.
fn git_dated(dir: &Path, secs: i64, args: &[&str]) -> String {
	let date = format!("@{secs} +0000");
	let out = Command::new("git")
		.arg("-C")
		.arg(dir)
		.args([
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"-c",
			"core.hooksPath=/dev/null",
		])
		.args(args)
		.env("GIT_AUTHOR_DATE", &date)
		.env("GIT_COMMITTER_DATE", &date)
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.output()
		.expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

/// `rev_list_excluding` lists what git's `rev-list <include> ^<exclude>` lists, in its order, across
/// a merge, a side branch and an unrelated history.
#[tokio::test]
async fn rev_list_excluding_matches_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("rev-excl");
	let t = 1_700_000_000;
	git_dated(
		&work,
		t,
		&["init", "-q", "--object-format=sha256", "-b", "main"],
	);
	git_dated(&work, t + 1, &["commit", "-q", "--allow-empty", "-m", "c1"]);
	git_dated(&work, t + 2, &["commit", "-q", "--allow-empty", "-m", "c2"]);
	git_dated(&work, t + 2, &["branch", "side"]);
	git_dated(&work, t + 3, &["switch", "-q", "side"]);
	git_dated(&work, t + 3, &["commit", "-q", "--allow-empty", "-m", "s1"]);
	git_dated(&work, t + 5, &["commit", "-q", "--allow-empty", "-m", "s2"]);
	git_dated(&work, t + 5, &["switch", "-q", "main"]);
	git_dated(&work, t + 4, &["commit", "-q", "--allow-empty", "-m", "c3"]);
	git_dated(&work, t + 6, &["merge", "-q", "--no-ff", "-m", "m", "side"]);
	git_dated(&work, t + 7, &["commit", "-q", "--allow-empty", "-m", "c4"]);
	git_dated(&work, t + 8, &["switch", "-q", "--orphan", "lone"]);
	git_dated(&work, t + 8, &["commit", "-q", "--allow-empty", "-m", "o1"]);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	for (include, exclude) in [
		("main", "main~3"),
		("main", "side~1"),
		("side", "main"),
		("main", "lone"),
		("main~2", "side"),
		("lone", "main"),
	] {
		let ours: Vec<String> = repo
			.rev_list_excluding(
				&[repo.rev_parse(include).await.unwrap()],
				&[repo.rev_parse(exclude).await.unwrap()],
			)
			.await
			.unwrap()
			.iter()
			.map(|id| id.to_hex())
			.collect();
		let theirs: Vec<String> = git_dated(&work, t, &["rev-list", include, &format!("^{exclude}")])
			.lines()
			.map(str::to_owned)
			.collect();
		assert_eq!(ours, theirs, "{include} ^{exclude}");
	}
	std::fs::remove_dir_all(&work).ok();
}

/// With a commit dated before its parent, the walk takes the parent first as included, then learns
/// the excluded side reaches it: that commit and what it reaches are excluded after all, as git's
/// `rev-list` has it.
#[tokio::test]
async fn rev_list_excluding_matches_git_with_a_commit_dated_before_its_parent() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("rev-excl-skew");
	let t = 1_700_000_000;
	git_dated(
		&work,
		t,
		&["init", "-q", "--object-format=sha256", "-b", "a"],
	);
	git_dated(&work, t, &["commit", "-q", "--allow-empty", "-m", "c1"]);
	git_dated(
		&work,
		t + 20,
		&["commit", "-q", "--allow-empty", "-m", "c2"],
	);
	git_dated(&work, t + 20, &["branch", "b"]);
	git_dated(
		&work,
		t + 30,
		&["commit", "-q", "--allow-empty", "-m", "c4"],
	);
	git_dated(&work, t + 30, &["switch", "-q", "b"]);
	// Dated before its parent c2.
	git_dated(&work, t + 5, &["commit", "-q", "--allow-empty", "-m", "c3"]);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let ours: Vec<String> = repo
		.rev_list_excluding(
			&[repo.rev_parse("a").await.unwrap()],
			&[repo.rev_parse("b").await.unwrap()],
		)
		.await
		.unwrap()
		.iter()
		.map(|id| id.to_hex())
		.collect();
	let theirs: Vec<String> = git_dated(&work, t, &["rev-list", "a", "^b"])
		.lines()
		.map(str::to_owned)
		.collect();
	assert_eq!(ours, theirs);
	assert_eq!(ours.len(), 1);
	std::fs::remove_dir_all(&work).ok();
}

/// A random history of `count` commits, each branch `c<n>` at commit n: merges, and distinct commit
/// dates that run backwards up to `skew` steps (a child dated before its parent). Seeded, so a
/// failure repeats.
fn random_history(work: &Path, seed: u64, count: usize, skew: i64) {
	let mut state = seed;
	let mut next = move |bound: u64| {
		state = state
			.wrapping_mul(6_364_136_223_846_793_005)
			.wrapping_add(1_442_695_040_888_963_407);
		(state >> 33) % bound
	};
	git_dated(
		work,
		0,
		&["init", "-q", "--object-format=sha256", "-b", "c0"],
	);
	let mut used = std::collections::HashSet::new();
	let mut script = String::new();
	for n in 0..count {
		let date = loop {
			let jitter = next((2 * skew * 100 + 1) as u64) as i64 - skew * 100;
			let date = 1_700_000_000 + n as i64 * 100 + jitter;
			if used.insert(date) {
				break date;
			}
		};
		script.push_str(&format!(
			"commit refs/heads/c{n}\nmark :{}\ncommitter T <t@e> {date} +0000\ndata 4\nc{n:02}\n",
			n + 1
		));
		if n > 0 {
			let first = n - 1 - next(n.min(3) as u64) as usize;
			script.push_str(&format!("from :{}\n", first + 1));
			if n > 2 && next(10) < 3 {
				let second = next(n as u64) as usize;
				if second != first {
					script.push_str(&format!("merge :{}\n", second + 1));
				}
			}
		}
	}
	let mut child = Command::new("git")
		.arg("-C")
		.arg(work)
		.args(["fast-import", "--quiet"])
		.stdin(std::process::Stdio::piped())
		.spawn()
		.unwrap();
	std::io::Write::write_all(&mut child.stdin.take().unwrap(), script.as_bytes()).unwrap();
	assert!(child.wait().unwrap().success());
}

/// On random histories with merges and commits dated before their parents, `rev_list_excluding`
/// lists what `git rev-list <a> ^<b>` lists, in its order.
#[tokio::test]
async fn rev_list_excluding_matches_git_on_random_histories() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	for seed in 1..=6u64 {
		let work = unique_tmp("rev-excl-random");
		random_history(&work, seed, 30, 4);
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(work.join(".git")),
		)));
		let mut state = seed;
		for _ in 0..60 {
			state = state
				.wrapping_mul(2_862_933_555_777_941_757)
				.wrapping_add(3_037_000_493);
			let (a, b) = ((state >> 20) % 30, (state >> 40) % 30);
			let (a, b) = (format!("c{a}"), format!("c{b}"));
			let ours: Vec<String> = repo
				.rev_list_excluding(
					&[repo.rev_parse(&a).await.unwrap()],
					&[repo.rev_parse(&b).await.unwrap()],
				)
				.await
				.unwrap()
				.iter()
				.map(|id| id.to_hex())
				.collect();
			let theirs: Vec<String> = git_dated(&work, 0, &["rev-list", &a, &format!("^{b}")])
				.lines()
				.map(str::to_owned)
				.collect();
			assert_eq!(ours, theirs, "seed {seed}: {a} ^{b}");
		}
		std::fs::remove_dir_all(&work).ok();
	}
}

/// On random histories whose dates run backwards no further than the walks' margin,
/// `reachable_among`, given several tips in one call, maps each commit to the first of them that
/// `git merge-base --is-ancestor` says reaches it, and leaves out those none reach.
#[tokio::test]
async fn reachable_among_matches_git_on_random_histories() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	for seed in 1..=4u64 {
		let work = unique_tmp("reachable-random");
		random_history(&work, seed, 30, 2);
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(work.join(".git")),
		)));
		let mut ids = Vec::new();
		for n in 0..30 {
			ids.push(repo.rev_parse(&format!("c{n}")).await.unwrap());
		}
		let tips = [13usize, 29, 7, 20];
		let tip_ids: Vec<_> = tips.iter().map(|tip| ids[*tip]).collect();
		let found = repo.reachable_among(&tip_ids, &ids).await.unwrap();
		for (n, id) in ids.iter().enumerate() {
			let first = tips.iter().position(|tip| {
				Command::new("git")
					.arg("-C")
					.arg(&work)
					.args([
						"merge-base",
						"--is-ancestor",
						&format!("c{n}"),
						&format!("c{tip}"),
					])
					.status()
					.unwrap()
					.success()
			});
			assert_eq!(
				found.get(id).copied(),
				first,
				"seed {seed}: the first tip reaching c{n}"
			);
		}
		std::fs::remove_dir_all(&work).ok();
	}
}

/// `reachable_among` walks only history no older than the commits asked about: an older commit's
/// object, removed, is never read.
#[tokio::test]
async fn reachable_among_stops_below_the_commits_asked_about() {
	let work = unique_tmp("reachable-stop");
	let git_dir = work.join(".git");
	create_skeleton(&git_dir);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();
	let mut commits = Vec::new();
	for n in 0..20 {
		commits.push(make_commit(&repo, format!("{n}\n").as_bytes(), 1_700_000_000 + n * 100).await);
	}
	let old = commits[2].to_hex();
	std::fs::remove_file(git_dir.join("objects").join(&old[..2]).join(&old[2..])).unwrap();
	let found = repo
		.reachable_among(&[commits[19]], &[commits[17], commits[18]])
		.await
		.unwrap();
	assert_eq!(
		found,
		std::collections::HashMap::from([(commits[17], 0), (commits[18], 0)])
	);
	// A commit the tip doesn't reach: the walk still stops below it.
	let other = repo
		.reachable_among(&[commits[16]], &[commits[17]])
		.await
		.unwrap();
	assert!(other.is_empty());
	std::fs::remove_dir_all(&work).ok();
}

/// Commits made by `git fast-import` from `(branch, date, parent)` lines, each parent the index of
/// an earlier line.
fn imported(work: &Path, commits: &[(&str, i64, Option<usize>)]) {
	git_dated(work, 0, &["init", "-q", "--object-format=sha256"]);
	let mut script = String::new();
	for (n, (branch, date, parent)) in commits.iter().enumerate() {
		script.push_str(&format!(
			"commit refs/heads/{branch}\nmark :{}\ncommitter T <t@e> {date} +0000\ndata 3\nn{n}\n",
			n + 1
		));
		if let Some(parent) = parent {
			script.push_str(&format!("from :{}\n", parent + 1));
		}
	}
	let mut child = Command::new("git")
		.arg("-C")
		.arg(work)
		.args(["fast-import", "--quiet"])
		.stdin(std::process::Stdio::piped())
		.spawn()
		.unwrap();
	std::io::Write::write_all(&mut child.stdin.take().unwrap(), script.as_bytes()).unwrap();
	assert!(child.wait().unwrap().success());
}

/// An excluded chain whose dates run backwards (each parent newer than its child) down to a commit
/// already listed: the walk goes on while what's queued is no older than that commit, past the
/// usual margin, until the chain reaches it; as git's `rev-list` does.
#[tokio::test]
async fn rev_list_excluding_follows_an_excluded_chain_dated_backwards() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("rev-excl-backwards");
	let t = 1_700_000_000;
	// 0: the root, listed early; 1: the included tip; 2..=9: the excluded chain, its tip (9) oldest,
	// each parent newer than its child, down to the root.
	let mut commits = vec![("a", t + 500, None), ("a", t + 1000, Some(0))];
	commits.push(("b", t + 670, Some(0)));
	for n in 0..7 {
		commits.push(("b", t + 660 - 10 * n, Some(commits.len() - 1)));
	}
	commits.push(("b", t + 400, Some(commits.len() - 1)));
	imported(&work, &commits);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let ours: Vec<String> = repo
		.rev_list_excluding(
			&[repo.rev_parse("a").await.unwrap()],
			&[repo.rev_parse("b").await.unwrap()],
		)
		.await
		.unwrap()
		.iter()
		.map(|id| id.to_hex())
		.collect();
	let theirs: Vec<String> = git_dated(&work, 0, &["rev-list", "a", "^b"])
		.lines()
		.map(str::to_owned)
		.collect();
	assert_eq!(ours, theirs);
	assert_eq!(ours.len(), 1);
	std::fs::remove_dir_all(&work).ok();
}

/// A commit reached only through a child dated before it: `reachable_among` takes its margin past
/// the oldest commit asked about, and finds it.
#[tokio::test]
async fn reachable_among_looks_past_a_child_dated_before_its_parent() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("reachable-backwards");
	let t = 1_700_000_000;
	imported(
		&work,
		&[
			("c", t + 600, None),
			("y", t + 400, Some(0)),
			("tip", t + 1000, Some(1)),
		],
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let c = repo.rev_parse("c").await.unwrap();
	let found = repo
		.reachable_among(&[repo.rev_parse("tip").await.unwrap()], &[c])
		.await
		.unwrap();
	assert_eq!(found, std::collections::HashMap::from([(c, 0)]));
	std::fs::remove_dir_all(&work).ok();
}

/// An excluded chain dated backwards that reaches a listed commit only at its far end: the exclusion
/// passes through each commit as it's read (as git's `process_parents` does), so the listed commit
/// is excluded although the margin runs out first.
#[tokio::test]
async fn rev_list_excluding_excludes_through_commits_only_read() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("rev-excl-read");
	let t = 1_700_000_000;
	// 0: C (700), 1: I (1000) on a; then E's chain from C: 100, 200, 300, 400, 500, E (600) on b.
	let mut commits = vec![("a", t + 700, None), ("a", t + 1000, Some(0))];
	commits.push(("b", t + 100, Some(0)));
	for date in [200, 300, 400, 500, 600] {
		commits.push(("b", t + date, Some(commits.len() - 1)));
	}
	imported(&work, &commits);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let ours: Vec<String> = repo
		.rev_list_excluding(
			&[repo.rev_parse("a").await.unwrap()],
			&[repo.rev_parse("b").await.unwrap()],
		)
		.await
		.unwrap()
		.iter()
		.map(|id| id.to_hex())
		.collect();
	let theirs: Vec<String> = git_dated(&work, 0, &["rev-list", "a", "^b"])
		.lines()
		.map(str::to_owned)
		.collect();
	assert_eq!(ours, theirs);
	assert_eq!(ours.len(), 1);
	std::fs::remove_dir_all(&work).ok();
}

/// A commit asked about, queued just as the margin would run out: `reachable_among` looks at what's
/// queued, not only at what it took last, and finds it.
#[tokio::test]
async fn reachable_among_finds_a_commit_queued_as_the_margin_ends() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("reachable-queued");
	let t = 1_700_000_000;
	// W (600), then 100..500 above it, then T (1000): each child older than W but T.
	let mut commits = vec![("w", t + 600, None)];
	for date in [100, 200, 300, 400, 500] {
		commits.push(("tip", t + date, Some(commits.len() - 1)));
	}
	commits.push(("tip", t + 1000, Some(commits.len() - 1)));
	imported(&work, &commits);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let w = repo.rev_parse("w").await.unwrap();
	let found = repo
		.reachable_among(&[repo.rev_parse("tip").await.unwrap()], &[w])
		.await
		.unwrap();
	assert!(
		Command::new("git")
			.arg("-C")
			.arg(&work)
			.args(["merge-base", "--is-ancestor", "w", "tip"])
			.status()
			.unwrap()
			.success()
	);
	assert_eq!(found, std::collections::HashMap::from([(w, 0)]));
	std::fs::remove_dir_all(&work).ok();
}

/// A chain of commits all made in the same second, each one a tip, the newest given first: every
/// commit is reached first by the newest, though which of equal dates the walk takes first is down
/// to their ids, and a commit asked about can be taken before the newest is.
#[tokio::test]
async fn reachable_among_names_the_first_tip_among_commits_of_one_date() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("reachable-one-date");
	let t = 1_700_000_000;
	let mut commits = vec![("c", t, None)];
	for _ in 1..12 {
		commits.push(("c", t, Some(commits.len() - 1)));
	}
	imported(&work, &commits);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let mut chain = Vec::new();
	for back in 0..12 {
		chain.push(repo.rev_parse(&format!("c~{back}")).await.unwrap());
	}
	// The newest first, then the rest oldest first.
	let mut tips = vec![chain[0]];
	tips.extend(chain[1..].iter().rev().copied());
	// Asked about all at once, and about each alone.
	let found = repo.reachable_among(&tips, &chain).await.unwrap();
	let expected: std::collections::HashMap<_, _> = chain.iter().map(|id| (*id, 0)).collect();
	assert_eq!(found, expected);
	for (back, id) in chain.iter().enumerate() {
		let found = repo.reachable_among(&tips, &[*id]).await.unwrap();
		assert_eq!(
			found,
			std::collections::HashMap::from([(*id, 0)]),
			"c~{back}"
		);
	}
	std::fs::remove_dir_all(&work).ok();
}

/// A packed branch since made a symbolic ref (a loose `ref:` file) isn't listed with its old packed
/// value: the loose file overrides it, and symbolic refs aren't listed, as git's `for-each-ref`
/// shows no stale value for it.
#[tokio::test]
async fn a_ref_made_symbolic_over_its_packed_value_is_not_listed() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("refs-symbolic-over-packed");
	git_dated(
		&work,
		0,
		&["init", "-q", "--object-format=sha256", "-b", "main"],
	);
	git_dated(
		&work,
		1_700_000_000,
		&["commit", "-q", "--allow-empty", "-m", "one"],
	);
	git_dated(&work, 1_700_000_000, &["branch", "alias"]);
	git_dated(&work, 1_700_000_000, &["pack-refs", "--all"]);
	git_dated(
		&work,
		1_700_000_100,
		&["commit", "-q", "--allow-empty", "-m", "two"],
	);
	git_dated(
		&work,
		0,
		&["symbolic-ref", "refs/heads/alias", "refs/heads/main"],
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let listed: Vec<String> = repo
		.refs()
		.list("refs/heads/")
		.await
		.unwrap()
		.into_iter()
		.map(|(name, _)| name)
		.collect();
	assert_eq!(listed, ["refs/heads/main"]);
	std::fs::remove_dir_all(&work).ok();
}

/// Files under refs/ whose names git would reject (an editor's `main~`, a `.lock` file, packed or
/// loose, direct or symbolic) aren't listed, as git's `for-each-ref` skips them.
#[tokio::test]
async fn refs_with_names_git_rejects_are_not_listed() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("refs-broken-names");
	git_dated(
		&work,
		0,
		&["init", "-q", "--object-format=sha256", "-b", "main"],
	);
	git_dated(
		&work,
		1_700_000_000,
		&["commit", "-q", "--allow-empty", "-m", "one"],
	);
	git_dated(&work, 1_700_000_000, &["tag", "kept"]);
	git_dated(&work, 1_700_000_000, &["pack-refs", "--all"]);
	let id = git_dated(&work, 0, &["rev-parse", "HEAD"]);
	let git_dir = work.join(".git");
	let mut packed = std::fs::read_to_string(git_dir.join("packed-refs")).unwrap();
	packed.push_str(&format!(
		"{0} refs/heads/packed~\n{0} refs/tags/packed.lock\n",
		id.trim()
	));
	std::fs::write(git_dir.join("packed-refs"), packed).unwrap();
	std::fs::write(git_dir.join("refs/heads/main~"), &id).unwrap();
	std::fs::write(git_dir.join("refs/heads/main.lock"), &id).unwrap();
	std::fs::create_dir_all(git_dir.join("refs/odd")).unwrap();
	std::fs::write(git_dir.join("refs/odd/a..b"), &id).unwrap();
	std::fs::write(git_dir.join("refs/heads/alias~"), "ref: refs/heads/main\n").unwrap();
	std::fs::write(git_dir.join("refs/heads/alias"), "ref: refs/heads/main\n").unwrap();
	let by_git: Vec<String> = git_dated(&work, 0, &["for-each-ref", "--format=%(refname)"])
		.lines()
		.map(str::to_owned)
		.collect();
	assert_eq!(
		by_git,
		["refs/heads/alias", "refs/heads/main", "refs/tags/kept"]
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(git_dir),
	)));
	let listed: Vec<String> = repo
		.refs()
		.list("refs/")
		.await
		.unwrap()
		.into_iter()
		.map(|(name, _)| name)
		.collect();
	assert_eq!(listed, ["refs/heads/main", "refs/tags/kept"]);
	let symbolic: Vec<String> = repo
		.refs()
		.symbolic_refs("refs/")
		.await
		.unwrap()
		.into_iter()
		.map(|(name, _)| name)
		.collect();
	assert_eq!(symbolic, ["refs/heads/alias"]);
	std::fs::remove_dir_all(&work).ok();
}

/// A commit whose text isn't UTF-8 (a Latin-1 message and author under git's `encoding` header) has
/// its tree read, and history walked through it, as git reads them: none of it needs its text.
/// Unix only: the Latin-1 name is passed to git in an environment variable as bytes.
#[cfg(unix)]
#[tokio::test]
async fn a_commit_s_tree_and_history_are_read_whatever_its_text_s_encoding() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("latin1-history");
	git_dated(
		&work,
		0,
		&["init", "-q", "--object-format=sha256", "-b", "main"],
	);
	git_dated(
		&work,
		1_700_000_000,
		&["commit", "-q", "--allow-empty", "-m", "one"],
	);
	std::fs::write(work.join("a.txt"), b"A\n").unwrap();
	git_dated(&work, 0, &["add", "a.txt"]);
	std::fs::write(work.join("msg"), b"caf\xe9\n").unwrap();
	let made = Command::new("git")
		.arg("-C")
		.arg(&work)
		.env(
			"GIT_AUTHOR_NAME",
			<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"Andr\xe9"),
		)
		.env("GIT_AUTHOR_EMAIL", "a@e")
		.env("GIT_AUTHOR_DATE", "@1700000060 +0000")
		.env("GIT_COMMITTER_DATE", "@1700000060 +0000")
		.args([
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"-c",
			"i18n.commitEncoding=ISO-8859-1",
		])
		.args(["commit", "-q", "-F", "msg"])
		.output()
		.unwrap();
	assert!(made.status.success(), "{made:?}");
	git_dated(
		&work,
		1_700_000_120,
		&["commit", "-q", "--allow-empty", "-m", "three"],
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let latin1 = repo.rev_parse("HEAD~1").await.unwrap();
	let tree = git_dated(&work, 0, &["rev-parse", "HEAD~1^{tree}"]);
	assert_eq!(
		repo.commit_tree(latin1).await.unwrap().to_hex(),
		tree.trim()
	);
	assert_eq!(
		repo.peel_to_tree(latin1).await.unwrap().to_hex(),
		tree.trim()
	);
	let three = repo.rev_parse("HEAD").await.unwrap();
	let one = repo.rev_parse("HEAD~2").await.unwrap();
	let listed: Vec<String> = repo
		.rev_list_excluding(&[three], &[one])
		.await
		.unwrap()
		.iter()
		.map(|id| id.to_hex())
		.collect();
	let by_git: Vec<String> = git_dated(&work, 0, &["rev-list", "HEAD", "^HEAD~2"])
		.lines()
		.map(str::to_owned)
		.collect();
	assert_eq!(listed, by_git);
	assert_eq!(
		repo.reachable_among(&[three], &[latin1]).await.unwrap(),
		std::collections::HashMap::from([(latin1, 0)])
	);
	std::fs::remove_dir_all(&work).ok();
}

/// An update along the chain HEAD had is refused once HEAD leads another way, even to a branch at
/// the same commit, so neither branch moves; along the chain HEAD still has, the branch at its end
/// moves.
#[tokio::test]
async fn an_update_along_a_chain_is_refused_once_head_leads_another_way() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("update-along");
	git_dated(
		&work,
		0,
		&["init", "-q", "--object-format=sha256", "-b", "main"],
	);
	git_dated(
		&work,
		1_700_000_000,
		&["commit", "-q", "--allow-empty", "-m", "one"],
	);
	git_dated(
		&work,
		1_700_000_060,
		&["commit", "-q", "--allow-empty", "-m", "two"],
	);
	git_dated(&work, 0, &["branch", "other"]);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	let chain = repo.refs().symbolic_chain("HEAD").await.unwrap();
	assert_eq!(chain, ["HEAD", "refs/heads/main"]);
	let one = repo.rev_parse("HEAD~1").await.unwrap();
	let two = repo.rev_parse("HEAD").await.unwrap();
	git_dated(&work, 0, &["symbolic-ref", "HEAD", "refs/heads/other"]);
	assert!(matches!(
		repo
			.refs()
			.update_ref_along(&chain, one, Some(two), ReflogIntent::Skip)
			.await,
		Err(gitana_repository::RepositoryError::RefMoved { .. })
	));
	for branch in ["main", "other"] {
		assert_eq!(
			git_dated(&work, 0, &["rev-parse", branch]).trim(),
			two.to_hex()
		);
	}
	git_dated(&work, 0, &["symbolic-ref", "HEAD", "refs/heads/main"]);
	repo
		.refs()
		.update_ref_along(&chain, one, Some(two), ReflogIntent::Skip)
		.await
		.unwrap();
	assert_eq!(
		git_dated(&work, 0, &["rev-parse", "main"]).trim(),
		one.to_hex()
	);
	std::fs::remove_dir_all(&work).ok();
}

/// A symbolic ref written `ref:` with no space after it, which git accepts, is read as symbolic.
#[tokio::test]
async fn a_symbolic_ref_without_the_space_is_read_as_git_reads_it() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("refs-symbolic-no-space");
	git_dated(
		&work,
		0,
		&["init", "-q", "--object-format=sha256", "-b", "main"],
	);
	git_dated(
		&work,
		1_700_000_000,
		&["commit", "-q", "--allow-empty", "-m", "one"],
	);
	std::fs::write(work.join(".git/HEAD"), "ref:refs/heads/main\n").unwrap();
	assert_eq!(
		git_dated(&work, 0, &["symbolic-ref", "HEAD"]).trim(),
		"refs/heads/main"
	);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(work.join(".git")),
	)));
	assert_eq!(
		repo.refs().read_symbolic("HEAD").await.unwrap().as_deref(),
		Some("refs/heads/main")
	);
	std::fs::remove_dir_all(&work).ok();
}

/// The walk stops once only excluded history is queued: an old commit's object, removed, is never
/// read (a full `rev_list` needs it), so a short range costs a short walk.
#[tokio::test]
async fn rev_list_excluding_stops_before_the_excluded_history() {
	let work = unique_tmp("rev-excl-stop");
	let git_dir = work.join(".git");
	create_skeleton(&git_dir);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();
	let mut commits = Vec::new();
	for n in 0..20 {
		commits.push(make_commit(&repo, format!("{n}\n").as_bytes(), 1_700_000_000 + n * 100).await);
	}
	let old = commits[2].to_hex();
	std::fs::remove_file(git_dir.join("objects").join(&old[..2]).join(&old[2..])).unwrap();
	assert!(repo.rev_list(&[commits[19]]).await.is_err());
	assert_eq!(
		repo
			.rev_list_excluding(&[commits[19]], &[commits[18]])
			.await
			.unwrap(),
		vec![commits[19]]
	);
	std::fs::remove_dir_all(&work).ok();
}

/// `logs_updates_of` follows the rule the store's own writes follow: git's
/// `core.logAllRefUpdates` (on by default in a repository with a work tree, for HEAD and branches),
/// and any reflog that already exists.
#[tokio::test]
async fn logs_updates_of_follows_core_log_all_ref_updates() {
	let work = unique_tmp("logs-updates");
	let git_dir = work.join(".git");
	create_skeleton(&git_dir);
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	repo.init().await.unwrap();
	let logs = |name: &'static str| {
		let repo = &repo;
		async move { repo.refs().logs_updates_of(name).await.unwrap() }
	};
	assert!(logs("HEAD").await);
	assert!(logs("refs/heads/main").await);
	assert!(!logs("refs/tags/v1").await);
	std::fs::write(
		git_dir.join("config"),
		"[core]\n\tlogallrefupdates = false\n",
	)
	.unwrap();
	assert!(!logs("HEAD").await);
	std::fs::create_dir_all(git_dir.join("logs")).unwrap();
	std::fs::write(git_dir.join("logs/HEAD"), "").unwrap();
	assert!(logs("HEAD").await);
	assert!(!logs("refs/heads/main").await);
	std::fs::write(
		git_dir.join("config"),
		"[core]\n\tlogallrefupdates = always\n",
	)
	.unwrap();
	assert!(logs("refs/tags/v1").await);
	std::fs::remove_dir_all(&work).ok();
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
