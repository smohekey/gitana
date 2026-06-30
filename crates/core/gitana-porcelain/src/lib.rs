//! Porcelain operations — git's user-facing commands as engine functions that *return data* (the CLI
//! adapter renders it). Each is generic over the file store and hash algorithm, so operations are
//! backend-agnostic and unit-testable without driving the CLI.

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_worktree::WorkTree;

/// Record a commit from the staged index on the current branch, returning the new commit id.
///
/// Refuses an unmerged or empty index *before* resolving identity, as git does — so a no-op commit
/// reports "nothing to commit" rather than an identity error. `identity` yields the git author and
/// committer lines (`Name <email> seconds ±hhmm`); it is only invoked once a commit will actually be
/// made, so the caller can resolve `GIT_*` / config lazily.
pub async fn commit<F: FileStore, H: HashAlgorithm>(
	wt: &WorkTree<F, H>,
	message: &str,
	identity: impl AsyncFnOnce() -> Result<(String, String)>,
) -> Result<ObjectId<H>> {
	let index = wt.load_index()?;
	// An unmerged index would silently drop conflicted paths (they have no stage-0 entry) from the
	// tree, so refuse — as git does — until they are resolved.
	if index.has_conflicts() {
		bail!(
			"committing is not possible because you have unmerged files; resolve them and mark resolution with `gta add`/`gta rm`"
		);
	}
	let entries = index.tree_entries();
	if entries.is_empty() {
		bail!("nothing to commit (empty index)");
	}

	let (author, committer) = identity().await?;
	let repo = wt.repository();
	let tree = repo.write_tree(&entries).await?;
	let message = if message.ends_with('\n') {
		message.to_owned()
	} else {
		format!("{message}\n")
	};
	Ok(
		repo
			.commit_on_head(tree, &author, &committer, &message)
			.await?,
	)
}

#[cfg(test)]
mod tests {
	use gitana_file_store_local::LocalFileStore;
	use gitana_object::Sha256;
	use gitana_object_store::ObjectStore;
	use gitana_repository::Repository;
	use gitana_worktree::{Index, IndexEntry, Stat, WorkTree};

	use super::*;

	const WHO: &str = "A U Thor <a@example.com> 0 +0000";

	/// A successful identity resolver.
	async fn who() -> Result<(String, String)> {
		Ok((WHO.to_owned(), WHO.to_owned()))
	}

	/// A fresh repository (config + an unborn `main`) with a work tree, over a temp `LocalFileStore`.
	async fn fixture() -> (tempfile::TempDir, WorkTree<LocalFileStore, Sha256>) {
		let dir = tempfile::TempDir::new().unwrap();
		let git_dir = dir.path().join(".git");
		std::fs::create_dir_all(&git_dir).unwrap();
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::new(&git_dir)));
		repo.init().await.unwrap();
		let wt = WorkTree::new(repo, dir.path().to_path_buf(), git_dir);
		(dir, wt)
	}

	fn stage(index: &mut Index<Sha256>, path: &str, oid: ObjectId<Sha256>) {
		index.upsert(IndexEntry {
			stat: Stat::default(),
			mode: 0o100644,
			oid,
			stage: 0,
			assume_valid: false,
			path: path.to_owned(),
		});
	}

	#[tokio::test]
	async fn records_the_staged_tree_on_head() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"hello\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		wt.save_index(&index).unwrap();

		let id = commit(&wt, "first", who).await.unwrap();

		// The branch now points at the commit, whose tree holds the staged blob.
		assert_eq!(
			wt.repository()
				.refs()
				.resolve("refs/heads/main")
				.await
				.unwrap(),
			Some(id)
		);
		let tree = wt.repository().commit_tree(id).await.unwrap();
		let entries = wt.repository().read_tree(tree).await.unwrap();
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].0, "f.txt");
		assert_eq!(entries[0].2, blob);
	}

	#[tokio::test]
	async fn refuses_an_empty_index_before_resolving_identity() {
		let (_dir, wt) = fixture().await;
		// The identity resolver must not run for a no-op commit (regression: it ran first, so an
		// unconfigured identity masked "nothing to commit").
		let resolved = std::cell::Cell::new(false);
		let err = commit(&wt, "x", async || {
			resolved.set(true);
			who().await
		})
		.await
		.unwrap_err();
		assert!(err.to_string().contains("nothing to commit"), "{err}");
		assert!(
			!resolved.get(),
			"identity resolved before the empty-index guard"
		);
	}

	#[tokio::test]
	async fn refuses_an_unmerged_index() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"x\n").await.unwrap();
		let mut index = Index::new();
		index.record_conflict("f.txt", Some((0o100644, blob)), None, None); // a stage-1 entry
		wt.save_index(&index).unwrap();

		let err = commit(&wt, "x", who).await.unwrap_err();
		assert!(err.to_string().contains("unmerged files"), "{err}");
	}
}
