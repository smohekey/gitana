//! End-to-end proof of the working-tree porcelain over the third (work-dir) descriptor.
//!
//! A native gitana lays down a one-commit repository (objects/refs/config/HEAD) in a *git dir*.
//! The component — instantiated with **no preopens** — is granted that git dir (as both git and
//! common dir, an ordinary repository) plus a separate empty *work dir*, all as `wasi:filesystem`
//! descriptors. Through them it must:
//!
//!   checkout the branch → materialise the tree into the work dir + index,
//!   observe an edit + a new file via `status`,
//!   `add` them, then `commit` — landing a new commit on the branch.
//!
//! The host reads the work dir off disk after checkout, and re-opens the git dir natively after
//! commit, to check every step against the native oracle — in both hash formats.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use anyhow::{Result, anyhow};
use gitana_object::{HashAlgorithm, ObjectId, Sha1, Sha256};
use gitana_repository::{FileMode, ReflogIntent, TreeBuildEntry};

use self::support::{AUTHOR, Session, committer, native_repo};

/// Build a one-commit ordinary repository natively in `git_dir`: `hello.txt` = "hello v1\n",
/// `dir/inner.txt` = "inner\n", and an executable `tool` on `refs/heads/main`, with a symbolic
/// `HEAD → main`. Returns the commit id (hex). The executable entry exercises the WASI degradation:
/// the component cannot set the exec bit, so a fresh checkout must still read as clean.
async fn seed_repo<H: HashAlgorithm>(git_dir: &std::path::Path) -> Result<String> {
	let repo = native_repo::<H>(git_dir)?;
	repo.init().await?;
	let hello = repo.write_blob(b"hello v1\n").await?;
	let inner = repo.write_blob(b"inner\n").await?;
	let tool = repo.write_blob(b"#!/bin/sh\nexit 0\n").await?;
	let entry = |path: &str, mode: FileMode, id: ObjectId<H>| TreeBuildEntry {
		path: path.to_owned(),
		mode,
		id,
	};
	let tree = repo
		.write_tree(&[
			entry("dir/inner.txt", FileMode::Regular, inner),
			entry("hello.txt", FileMode::Regular, hello),
			entry("tool", FileMode::Executable, tool),
		])
		.await?;
	let commit = repo
		.create_commit(tree, Vec::new(), AUTHOR, &committer(0), "seed\n")
		.await?;
	repo
		.refs()
		.update_ref("refs/heads/main", commit, None, ReflogIntent::Skip)
		.await?;
	Ok(commit.to_hex())
}

/// Read the blob at `path` in the tree of commit `spec`, natively.
async fn blob_in_commit<H: HashAlgorithm>(
	git_dir: &std::path::Path,
	spec: &str,
	path: &str,
) -> Result<Vec<u8>> {
	let repo = native_repo::<H>(git_dir)?;
	let commit = repo.rev_parse(spec).await?;
	let tree = repo.commit_tree(commit).await?;
	let entries = repo.read_tree(tree).await?;
	let (_, _, id) = entries
		.into_iter()
		.find(|(entry_path, _, _)| entry_path == path)
		.ok_or_else(|| anyhow!("{path} not in the tree of {spec}"))?;
	Ok(repo.read_blob(id).await?)
}

async fn worktree_porcelain_round_trip<H: HashAlgorithm>() -> Result<()> {
	let git = tempfile::tempdir()?;
	let git_dir = git.path();
	let seed = seed_repo::<H>(git_dir).await?;

	let work = tempfile::tempdir()?;
	let work_dir = work.path();

	// Ordinary repository: the git dir is also the common dir.
	let mut session = Session::open_worktree(git_dir, git_dir, work_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// -- checkout: the branch tip's tree materialises into the (empty) work dir and index.
	porcelain
		.call_checkout(&mut *store, handle, "main", false)
		.await?
		.map_err(|error| anyhow!("checkout: {error:?}"))?;
	assert_eq!(std::fs::read(work_dir.join("hello.txt"))?, b"hello v1\n");
	assert_eq!(std::fs::read(work_dir.join("dir/inner.txt"))?, b"inner\n");
	assert_eq!(
		std::fs::read(work_dir.join("tool"))?,
		b"#!/bin/sh\nexit 0\n"
	);

	// -- status: right after checkout the tree is clean. In particular the executable `tool` must
	//    not read as dirty even though the component could not set its exec bit — the index records
	//    the mode the capability reports (`100644`), git's `core.fileMode=false`.
	let clean = porcelain
		.call_status(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("status: {error:?}"))?;
	assert!(
		clean.changed.is_empty() && clean.untracked.is_empty(),
		"expected a clean status, got {clean:?}"
	);

	// -- commit on a clean tree is refused (the index tree still equals HEAD's): git's "nothing to
	//    commit", not a duplicate empty commit.
	let refused = porcelain
		.call_commit(&mut *store, handle, "noop\n", AUTHOR, &committer(5))
		.await?;
	match refused {
		Err(gitana_repo_host::exports::gitana::repo::porcelain::RepoError::Invalid(message)) => {
			assert!(
				message.contains("nothing to commit"),
				"unexpected commit error: {message}"
			);
		}
		other => anyhow::bail!("expected a nothing-to-commit refusal, got {other:?}"),
	}

	// -- edit an existing file and add an untracked one, on disk (the host owns the work dir).
	std::fs::write(work_dir.join("hello.txt"), b"hello v2\n")?;
	std::fs::write(work_dir.join("new.txt"), b"new\n")?;

	// -- status sees the unstaged edit (` M`) and the untracked file.
	let dirty = porcelain
		.call_status(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("status: {error:?}"))?;
	let hello = dirty
		.changed
		.iter()
		.find(|entry| entry.path == "hello.txt")
		.ok_or_else(|| anyhow!("hello.txt not in status: {dirty:?}"))?;
	assert_eq!((hello.index.as_str(), hello.worktree.as_str()), (" ", "M"));
	assert_eq!(dirty.untracked, vec!["new.txt".to_owned()]);

	// -- add: stage everything under the work-tree root (`.`, as `gta add .` does).
	porcelain
		.call_add(&mut *store, handle, &[".".to_owned()], "", false)
		.await?
		.map_err(|error| anyhow!("add: {error:?}"))?;

	// -- status now shows both staged (X column), nothing untracked.
	let staged = porcelain
		.call_status(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("status: {error:?}"))?;
	assert!(staged.untracked.is_empty(), "still untracked: {staged:?}");
	let code = |path: &str| {
		staged
			.changed
			.iter()
			.find(|entry| entry.path == path)
			.map(|entry| (entry.index.as_str(), entry.worktree.as_str()))
	};
	assert_eq!(code("hello.txt"), Some(("M", " ")));
	assert_eq!(code("new.txt"), Some(("A", " ")));

	// -- commit: record the staged tree on main; the host supplies the identity lines.
	let new_commit = porcelain
		.call_commit(&mut *store, handle, "edit\n", AUTHOR, &committer(10))
		.await?
		.map_err(|error| anyhow!("commit: {error:?}"))?;

	// -- oracle: main advanced to the new commit, whose parent is the seed and whose tree holds
	//    the edited + added content alongside the untouched file.
	let repo = native_repo::<H>(git_dir)?;
	assert_eq!(
		repo.refs().resolve("refs/heads/main").await?,
		Some(ObjectId::from_hex(&new_commit)?)
	);
	// The new commit sits on top of the seed (its first parent), i.e. it advanced the branch.
	assert_eq!(
		repo.rev_parse(&format!("{new_commit}^")).await?,
		ObjectId::from_hex(&seed)?
	);
	assert_eq!(
		blob_in_commit::<H>(git_dir, &new_commit, "hello.txt").await?,
		b"hello v2\n"
	);
	assert_eq!(
		blob_in_commit::<H>(git_dir, &new_commit, "new.txt").await?,
		b"new\n"
	);
	assert_eq!(
		blob_in_commit::<H>(git_dir, &new_commit, "dir/inner.txt").await?,
		b"inner\n"
	);

	// -- delete a tracked file on disk and `add .`: the deletion is staged (git 2.0 `add .`), shows
	//    as a staged removal, and committing drops it from the tree.
	std::fs::remove_file(work_dir.join("hello.txt"))?;
	porcelain
		.call_add(&mut *store, handle, &[".".to_owned()], "", false)
		.await?
		.map_err(|error| anyhow!("add: {error:?}"))?;
	let deleted = porcelain
		.call_status(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("status: {error:?}"))?;
	assert_eq!(
		deleted
			.changed
			.iter()
			.find(|entry| entry.path == "hello.txt")
			.map(|entry| (entry.index.as_str(), entry.worktree.as_str())),
		Some(("D", " ")),
		"hello.txt should be a staged deletion: {deleted:?}"
	);
	let dropped = porcelain
		.call_commit(&mut *store, handle, "drop hello\n", AUTHOR, &committer(20))
		.await?
		.map_err(|error| anyhow!("commit: {error:?}"))?;
	let repo = native_repo::<H>(git_dir)?;
	let tree = repo.commit_tree(ObjectId::from_hex(&dropped)?).await?;
	let entries = repo.read_tree(tree).await?;
	assert!(
		!entries.iter().any(|(path, _, _)| path == "hello.txt"),
		"hello.txt must be gone from the committed tree: {entries:?}"
	);
	assert!(
		entries.iter().any(|(path, _, _)| path == "new.txt"),
		"new.txt must remain: {entries:?}"
	);

	Ok(())
}

/// The sparse-checkout surface over the component: `sparse-set`/`list`/`add`/`disable`, driving the
/// engine through the granted descriptors and — crucially — writing git's config (the common-dir
/// `config`'s `extensions.worktreeConfig`, and the per-worktree `config.worktree`) through them.
async fn worktree_sparse_round_trip<H: HashAlgorithm>() -> Result<()> {
	let git = tempfile::tempdir()?;
	let git_dir = git.path();
	seed_repo::<H>(git_dir).await?;
	let work = tempfile::tempdir()?;
	let work_dir = work.path();

	let mut session = Session::open_worktree(git_dir, git_dir, work_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// -- full checkout first: the `dir/` subtree and the root files all materialise.
	porcelain
		.call_checkout(&mut *store, handle, "main", false)
		.await?
		.map_err(|error| anyhow!("checkout: {error:?}"))?;
	assert!(work_dir.join("dir/inner.txt").exists());
	assert!(work_dir.join("hello.txt").exists());

	// -- sparse-set to the default cone (root files only): the out-of-cone `dir/` is omitted, roots stay.
	let outcome = porcelain
		.call_sparse_set(&mut *store, handle, &[], true)
		.await?
		.map_err(|error| anyhow!("sparse-set: {error:?}"))?;
	assert!(outcome.left_dirty.is_empty());
	assert!(
		!work_dir.join("dir").exists(),
		"the cone default (root files only) must omit dir/"
	);
	assert!(work_dir.join("hello.txt").exists(), "root files stay");

	// -- the component wrote git's config through the granted descriptors: the extension in the common
	//    `config`, the mode booleans in `config.worktree`, and the pattern file. (gitana renders config
	//    variable names lowercased — git reads them case-insensitively — so match case-insensitively.)
	assert!(
		std::fs::read_to_string(git_dir.join("config"))?
			.to_lowercase()
			.contains("worktreeconfig"),
		"extensions.worktreeConfig must be written to the common config"
	);
	assert!(
		std::fs::read_to_string(git_dir.join("config.worktree"))?
			.to_lowercase()
			.contains("sparsecheckout"),
		"core.sparseCheckout must be written to config.worktree"
	);
	assert_eq!(
		std::fs::read_to_string(git_dir.join("info/sparse-checkout"))?,
		"/*\n!/*/\n"
	);

	// -- list reflects the empty cone set.
	let listed = porcelain
		.call_sparse_list(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("sparse-list: {error:?}"))?
		.ok_or_else(|| anyhow!("expected sparse-checkout enabled"))?;
	assert!(listed.cone && listed.entries.is_empty(), "got {listed:?}");

	// -- a cone `sparse-set` with a glob metacharacter is rejected as `invalid` (the component applies the
	//    same literal-directory validation as the CLI, rather than rendering a broken `/*/` pattern file).
	match porcelain
		.call_sparse_set(&mut *store, handle, &["*".to_owned()], true)
		.await?
	{
		Err(gitana_repo_host::exports::gitana::repo::porcelain::RepoError::Invalid(message)) => {
			assert!(
				message.contains("pattern character"),
				"unexpected cone-metachar error: {message}"
			);
		}
		other => anyhow::bail!("expected a cone glob to be refused as invalid, got {other:?}"),
	}

	// -- a cone `sparse-set` naming a tracked *file* (not a directory) is rejected as `invalid`, the same
	//    index check the native CLI applies.
	match porcelain
		.call_sparse_set(&mut *store, handle, &["hello.txt".to_owned()], true)
		.await?
	{
		Err(gitana_repo_host::exports::gitana::repo::porcelain::RepoError::Invalid(message)) => {
			assert!(
				message.contains("not a directory"),
				"unexpected tracked-file cone error: {message}"
			);
		}
		other => {
			anyhow::bail!("expected a tracked-file cone entry to be refused as invalid, got {other:?}")
		}
	}

	// -- adding the now-excluded `dir` is refused as `invalid` (the engine's PathspecAdvisory is mapped
	//    to a precise WIT error, not a generic backend failure).
	match porcelain
		.call_add(&mut *store, handle, &["dir".to_owned()], "", false)
		.await?
	{
		Err(gitana_repo_host::exports::gitana::repo::porcelain::RepoError::Invalid(message)) => {
			assert!(
				message.contains("sparse-checkout"),
				"unexpected out-of-cone add error: {message}"
			);
		}
		other => anyhow::bail!("expected an out-of-cone add to be refused as invalid, got {other:?}"),
	}

	// -- add `dir`: the subtree is materialised, and list now names it.
	porcelain
		.call_sparse_add(&mut *store, handle, &["dir".to_owned()])
		.await?
		.map_err(|error| anyhow!("sparse-add: {error:?}"))?;
	assert_eq!(std::fs::read(work_dir.join("dir/inner.txt"))?, b"inner\n");
	let listed = porcelain
		.call_sparse_list(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("sparse-list: {error:?}"))?
		.ok_or_else(|| anyhow!("expected sparse-checkout enabled"))?;
	assert_eq!(listed.entries, vec!["dir".to_owned()]);

	// -- disable: the whole tree is restored and list reports no set.
	porcelain
		.call_sparse_disable(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("sparse-disable: {error:?}"))?;
	assert!(work_dir.join("dir/inner.txt").exists());
	let listed = porcelain
		.call_sparse_list(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("sparse-list: {error:?}"))?;
	assert!(listed.is_none(), "disabled sparse-checkout has no set");

	// -- an empty non-cone `sparse-set` applies git's non-cone default (root files only), not an empty
	//    pattern file that would omit even root files.
	porcelain
		.call_sparse_set(&mut *store, handle, &[], false)
		.await?
		.map_err(|error| anyhow!("sparse-set non-cone default: {error:?}"))?;
	assert!(
		work_dir.join("hello.txt").exists(),
		"root files stay under the non-cone default"
	);
	assert!(
		!work_dir.join("dir").exists(),
		"the non-cone default omits directories"
	);
	let listed = porcelain
		.call_sparse_list(&mut *store, handle)
		.await?
		.map_err(|error| anyhow!("sparse-list: {error:?}"))?
		.ok_or_else(|| anyhow!("expected sparse-checkout enabled"))?;
	assert!(!listed.cone, "the empty set is non-cone, got {listed:?}");

	Ok(())
}

#[tokio::test]
async fn sha256_worktree_porcelain_round_trip() -> Result<()> {
	worktree_porcelain_round_trip::<Sha256>().await
}

#[tokio::test]
async fn sha1_worktree_porcelain_round_trip() -> Result<()> {
	worktree_porcelain_round_trip::<Sha1>().await
}

#[tokio::test]
async fn sha256_worktree_sparse_round_trip() -> Result<()> {
	worktree_sparse_round_trip::<Sha256>().await
}

#[tokio::test]
async fn sha1_worktree_sparse_round_trip() -> Result<()> {
	worktree_sparse_round_trip::<Sha1>().await
}
