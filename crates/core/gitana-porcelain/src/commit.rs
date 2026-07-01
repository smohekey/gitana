//! `commit` — record a commit from the staged index on the current branch.

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_worktree::WorkTree;

use crate::Identity;

/// Record a commit from the staged index on the current branch, returning the new commit id.
///
/// Refuses an unmerged or empty index *before* resolving identity, as git does — so a no-op commit
/// reports "nothing to commit" rather than an identity error. `identity` is asked for the author and
/// committer lines only once a commit will actually be made, so the caller can resolve `GIT_*` /
/// config lazily.
pub async fn commit<F: FileStore, H: HashAlgorithm>(
	wt: &WorkTree<F, H>,
	message: &str,
	identity: &impl Identity,
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

	let author = identity.author().await?;
	let committer = identity.committer().await?;
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use gitana_object::Sha256;
	use gitana_worktree::Index;

	use super::*;
	use crate::test_support::{TestIdentity, fixture, stage};

	#[tokio::test]
	async fn records_the_staged_tree_on_head() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"hello\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		wt.save_index(&index).unwrap();

		let id = commit(&wt, "first", &TestIdentity::default())
			.await
			.unwrap();

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
		// The identity must not be resolved for a no-op commit (regression: it resolved first, so an
		// unconfigured identity masked "nothing to commit").
		let identity = TestIdentity::default();
		let err = commit(&wt, "x", &identity).await.unwrap_err();
		assert!(err.to_string().contains("nothing to commit"), "{err}");
		assert!(
			!identity.asked.get(),
			"identity resolved before the empty-index guard"
		);
	}

	#[tokio::test]
	async fn refuses_an_unmerged_index() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"x\n").await.unwrap();
		let mut index = Index::<Sha256>::new();
		index.record_conflict("f.txt", Some((0o100644, blob)), None, None); // a stage-1 entry
		wt.save_index(&index).unwrap();

		let err = commit(&wt, "x", &TestIdentity::default())
			.await
			.unwrap_err();
		assert!(err.to_string().contains("unmerged files"), "{err}");
	}
}
