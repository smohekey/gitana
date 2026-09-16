use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_object_store::ObjectStore;

use crate::{HeadState, HeadTransaction, RefStore, ReflogIntent, Repository, RepositoryError};

const HISTORY_PATHS: &[&str] = &["shallow", "rebase-merge", "rebase-apply", "sequencer"];

/// A locked, history-free repository ready to publish its first commit.
///
/// The transaction retains Gitana's repository-wide history lock together with `HEAD`, every
/// symbolic hop, the terminal ref, `ORIG_HEAD`, and `packed-refs`. Conforming repository writers
/// therefore cannot introduce another history root before [`publish_durable`](Self::publish_durable)
/// revalidates the repository and durably publishes the initial commit.
#[must_use = "a held InitialCommitTransaction retains repository history locks until publication or drop"]
pub struct InitialCommitTransaction<S, H: HashAlgorithm> {
	head: HeadTransaction<S, H>,
	terminal_ref: String,
}

impl<S, H> InitialCommitTransaction<S, H>
where
	S: FileStore + 'static,
	H: HashAlgorithm,
{
	pub(crate) async fn new(head: HeadTransaction<S, H>) -> Result<Self, RepositoryError> {
		let terminal_ref = head
			.head_chain
			.last()
			.expect("a HEAD transaction always contains HEAD")
			.clone();
		let mut transaction = Self { head, terminal_ref };
		transaction.ensure_history_free().await?;
		Ok(transaction)
	}

	/// Revalidate and durably publish the first commit before releasing the history lock.
	///
	/// The candidate must be a valid commit whose object graph has already crossed its durability
	/// barrier. This method publishes the captured unborn terminal ref, flushes that ref and the
	/// candidate's reachable graph, and only then permits another history mutation.
	pub async fn publish_durable(
		self,
		target: ObjectId<H>,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		let reflog = match reflog {
			ReflogIntent::Log { committer, message } => Some((committer.to_owned(), message.to_owned())),
			ReflogIntent::Skip => None,
		};

		#[cfg(not(target_arch = "wasm32"))]
		{
			match tokio::spawn(async move { self.publish_durable_inline(target, reflog).await }).await {
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self.publish_durable_inline(target, reflog).await
	}

	async fn publish_durable_inline(
		mut self,
		target: ObjectId<H>,
		reflog: Option<(String, String)>,
	) -> Result<(), RepositoryError> {
		self.ensure_history_free().await?;
		let repository = Repository::new(ObjectStore::new(self.head.files.shared_handle()));
		repository.commit_tree(target).await?;
		let reflog = match &reflog {
			Some((committer, message)) => ReflogIntent::Log { committer, message },
			None => ReflogIntent::Skip,
		};
		self.head.prepare_reset(target, reflog).await?;

		let history_lease = self
			.head
			.history_lease
			.take()
			.expect("an initial-commit transaction retains the history lock");
		self.head.finish().await?;

		let result = repository
			.durability_barrier_ref(&self.terminal_ref, target, &[])
			.await;
		drop(history_lease);
		result
	}

	async fn ensure_history_free(&mut self) -> Result<(), RepositoryError> {
		if !matches!(self.head.state, HeadState::Symbolic(_)) || self.head.tip.is_some() {
			return Err(RepositoryError::ExistingHistory);
		}

		let refs =
			RefStore::<_, H>::new(&self.head.files).with_effective_config(self.head.effective.as_ref());
		let has_out_of_chain_symbolic_ref = refs
			.symbolic_refs("refs/")
			.await?
			.iter()
			.any(|(name, _)| !self.head.head_chain.contains(name));
		if !refs.list("refs/").await?.is_empty() || has_out_of_chain_symbolic_ref {
			return Err(RepositoryError::ExistingHistory);
		}

		let files = &self.head.files;
		for path in files.list_prefix("").await? {
			if path != "HEAD" && is_pseudoref_name(&path) && !self.head.head_chain.contains(&path) {
				return Err(RepositoryError::ExistingHistory);
			}
		}
		if files.exists("logs").await? || files.is_dir("logs").await? {
			return Err(RepositoryError::ExistingHistory);
		}
		if files.exists("worktrees").await? || files.is_dir("worktrees").await? {
			return Err(RepositoryError::ExistingHistory);
		}
		for path in HISTORY_PATHS {
			if files.exists(path).await? || files.is_dir(path).await? {
				return Err(RepositoryError::ExistingHistory);
			}
		}

		Ok(())
	}
}

/// Git's top-level pseudoref syntax: one or more ASCII uppercase letters, underscores, or hyphens.
fn is_pseudoref_name(name: &str) -> bool {
	!name.is_empty()
		&& name
			.bytes()
			.all(|byte| byte.is_ascii_uppercase() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
	#[cfg(not(target_arch = "wasm32"))]
	use std::future::Future;
	#[cfg(not(target_arch = "wasm32"))]
	use std::task::{Context, Poll, Waker};

	use gitana_file_store::FileStore;
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::Sha256;
	use gitana_object_store::ObjectStore;

	use super::*;

	const IDENTITY: &str = "Forge Test <forge@example.invalid> 0 +0000";

	fn new_repository(files: MemoryFileStore) -> Repository<MemoryFileStore, Sha256> {
		Repository::new(ObjectStore::new(files))
	}

	async fn initial_commit(repository: &Repository<impl FileStore, Sha256>) -> ObjectId<Sha256> {
		let tree = repository.write_tree(&[]).await.unwrap();
		repository
			.create_commit(tree, Vec::new(), IDENTITY, IDENTITY, "Initial commit\n")
			.await
			.unwrap()
	}

	#[tokio::test]
	async fn initial_commit_transaction_publishes_durably() {
		let repository = new_repository(MemoryFileStore::new());
		repository.init().await.unwrap();
		let commit = initial_commit(&repository).await;
		repository
			.durability_barrier_object_graph(commit, &[])
			.await
			.unwrap();

		let transaction = repository.lock_initial_commit_transaction().await.unwrap();
		transaction
			.publish_durable(
				commit,
				ReflogIntent::Log {
					committer: IDENTITY,
					message: "commit (initial): Initial commit",
				},
			)
			.await
			.unwrap();

		assert_eq!(
			repository.refs().resolve_head().await.unwrap(),
			Some(commit)
		);
	}

	#[tokio::test]
	async fn initial_commit_transaction_requires_a_commit_target() {
		let files = MemoryFileStore::new();
		let repository = new_repository(files.shared_handle());
		repository.init().await.unwrap();
		let blob = repository.write_blob(b"not a commit").await.unwrap();
		let tree = repository.write_tree(&[]).await.unwrap();

		for target in [blob, tree] {
			repository
				.durability_barrier_object_graph(target, &[])
				.await
				.unwrap();
			let transaction = repository.lock_initial_commit_transaction().await.unwrap();
			let error = transaction
				.publish_durable(target, ReflogIntent::Skip)
				.await
				.unwrap_err();

			assert!(
				matches!(error, RepositoryError::InvalidRef(message) if message == format!("{target} is not a commit")),
				"unexpected publication result for {target}"
			);
			assert_eq!(repository.refs().resolve_head().await.unwrap(), None);
			assert_eq!(
				repository.refs().resolve("refs/heads/main").await.unwrap(),
				None
			);
			assert!(!files.exists("logs").await.unwrap());
		}
	}

	#[tokio::test]
	async fn initial_commit_transaction_rejects_retained_history_state() {
		for path in [
			"FETCH_HEAD",
			"MERGE_HEAD",
			"REBASE_ORIG_HEAD",
			"BISECT_HEAD",
			"AUTO_MERGE",
			"shallow",
		] {
			let files = MemoryFileStore::new();
			let repository = new_repository(files.shared_handle());
			repository.init().await.unwrap();
			files
				.write_path_replace(path, b"retained history\n")
				.await
				.unwrap();

			let error = repository
				.lock_initial_commit_transaction()
				.await
				.err()
				.unwrap();

			assert!(
				matches!(error, RepositoryError::ExistingHistory),
				"unexpected result for {path}: {error:?}"
			);
		}

		let files = MemoryFileStore::new();
		let repository = new_repository(files.shared_handle());
		repository.init().await.unwrap();
		files
			.write_path_replace("logs/refs/heads/deleted", b"retained reflog\n")
			.await
			.unwrap();
		assert!(matches!(
			repository
				.lock_initial_commit_transaction()
				.await
				.err()
				.unwrap(),
			RepositoryError::ExistingHistory
		));
	}

	#[tokio::test]
	async fn initial_commit_transaction_rejects_every_top_level_pseudoref() {
		for (name, symbolic) in [("CUSTOM-REF", false), ("CUSTOM_REF", true)] {
			let files = MemoryFileStore::new();
			let repository = new_repository(files.shared_handle());
			repository.init().await.unwrap();
			let old = initial_commit(&repository).await;
			if symbolic {
				repository
					.refs()
					.set_symbolic(name, "MISSING-REF", ReflogIntent::Skip)
					.await
					.unwrap();
			} else {
				repository
					.refs()
					.update_ref(name, old, None, ReflogIntent::Skip)
					.await
					.unwrap();
			}

			let error = repository
				.lock_initial_commit_transaction()
				.await
				.err()
				.unwrap();
			assert!(
				matches!(error, RepositoryError::ExistingHistory),
				"unexpected result for {name}: {error:?}"
			);
		}
	}

	#[tokio::test]
	async fn initial_commit_transaction_rejects_unresolved_symbolic_refs_outside_head_chain() {
		let files = MemoryFileStore::new();
		let repository = new_repository(files.shared_handle());
		repository.init().await.unwrap();
		repository
			.refs()
			.set_symbolic("refs/heads/alias", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();

		let error = repository
			.lock_initial_commit_transaction()
			.await
			.err()
			.unwrap();

		assert!(matches!(error, RepositoryError::ExistingHistory));
	}

	#[tokio::test]
	async fn initial_commit_transaction_allows_the_captured_unborn_symbolic_chain() {
		let files = MemoryFileStore::new();
		let repository = new_repository(files.shared_handle());
		repository.init().await.unwrap();
		let commit = initial_commit(&repository).await;
		repository
			.durability_barrier_object_graph(commit, &[])
			.await
			.unwrap();
		repository
			.refs()
			.set_symbolic("CUSTOM_REF", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();
		repository
			.refs()
			.set_symbolic("refs/heads/alias", "CUSTOM_REF", ReflogIntent::Skip)
			.await
			.unwrap();
		repository
			.refs()
			.set_symbolic("HEAD", "refs/heads/alias", ReflogIntent::Skip)
			.await
			.unwrap();

		let transaction = repository.lock_initial_commit_transaction().await.unwrap();
		transaction
			.publish_durable(commit, ReflogIntent::Skip)
			.await
			.unwrap();

		assert_eq!(
			repository.refs().resolve_head().await.unwrap(),
			Some(commit)
		);
		assert_eq!(
			repository.refs().resolve("refs/heads/main").await.unwrap(),
			Some(commit)
		);
	}

	#[tokio::test]
	async fn initial_commit_transaction_rejects_a_worktrees_namespace() {
		let files = MemoryFileStore::new();
		let repository = new_repository(files.shared_handle());
		repository.init().await.unwrap();
		files
			.write_path_replace("worktrees/other/HEAD", b"ref: refs/heads/other\n")
			.await
			.unwrap();

		let error = repository
			.lock_initial_commit_transaction()
			.await
			.err()
			.unwrap();
		assert!(matches!(error, RepositoryError::ExistingHistory));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn initial_commit_transaction_rejects_worktree_file_store_contexts() {
		use cap_std::ambient_authority;
		use cap_std::fs::Dir;
		use gitana_file_store_local::WorktreeFileStore;

		let root = std::env::temp_dir().join(format!("gitana-initial-linked-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&root);
		let primary = root.join("primary");
		std::fs::create_dir_all(primary.join("worktrees")).unwrap();
		let primary_common = Dir::open_ambient_dir(&primary, ambient_authority()).unwrap();
		let primary_worktree = Dir::open_ambient_dir(&primary, ambient_authority()).unwrap();
		let files = WorktreeFileStore::new(primary_common, primary_worktree);
		let repository = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repository.init().await.unwrap();
		let error = repository
			.lock_initial_commit_transaction()
			.await
			.err()
			.unwrap();
		assert!(matches!(error, RepositoryError::ExistingHistory));

		let common = root.join("common");
		let linked = common.join("worktrees/linked");
		std::fs::create_dir_all(&linked).unwrap();
		let common_dir = Dir::open_ambient_dir(&common, ambient_authority()).unwrap();
		let linked_dir = Dir::open_ambient_dir(&linked, ambient_authority()).unwrap();
		let files = WorktreeFileStore::new(common_dir, linked_dir);
		let repository = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repository.init().await.unwrap();

		let error = repository
			.lock_initial_commit_transaction()
			.await
			.err()
			.unwrap();
		assert!(matches!(error, RepositoryError::ExistingHistory));

		let _ = std::fs::remove_dir_all(root);
	}

	#[tokio::test]
	async fn initial_commit_transaction_excludes_other_history_writers() {
		let files = MemoryFileStore::new();
		let repository = new_repository(files.shared_handle());
		let other = new_repository(files.shared_handle());
		repository.init().await.unwrap();
		let commit = initial_commit(&repository).await;
		let transaction = repository.lock_initial_commit_transaction().await.unwrap();

		for result in [
			other
				.refs()
				.update_ref("refs/heads/other", commit, None, ReflogIntent::Skip)
				.await,
			other.start_merge(commit, "Merge in progress\n").await,
			other
				.refs()
				.append_reflog(
					"refs/heads/deleted",
					None,
					Some(commit),
					IDENTITY,
					"retained",
				)
				.await,
		] {
			assert!(matches!(result, Err(RepositoryError::HistoryLocked)));
		}

		drop(transaction);
		other
			.refs()
			.update_ref("refs/heads/other", commit, None, ReflogIntent::Skip)
			.await
			.unwrap();
	}

	#[tokio::test]
	async fn publication_rechecks_history_after_a_backend_bypass() {
		let files = MemoryFileStore::new();
		let repository = new_repository(files.shared_handle());
		repository.init().await.unwrap();
		let commit = initial_commit(&repository).await;
		let transaction = repository.lock_initial_commit_transaction().await.unwrap();
		files
			.write_path_replace("FETCH_HEAD", format!("{commit}\n").as_bytes())
			.await
			.unwrap();

		let error = transaction
			.publish_durable(commit, ReflogIntent::Skip)
			.await
			.unwrap_err();

		assert!(matches!(error, RepositoryError::ExistingHistory));
		assert_eq!(repository.refs().resolve_head().await.unwrap(), None);
	}

	#[tokio::test]
	async fn publication_rechecks_unresolved_symbolic_refs_after_a_backend_bypass() {
		let files = MemoryFileStore::new();
		let repository = new_repository(files.shared_handle());
		repository.init().await.unwrap();
		let commit = initial_commit(&repository).await;
		repository
			.durability_barrier_object_graph(commit, &[])
			.await
			.unwrap();
		let transaction = repository.lock_initial_commit_transaction().await.unwrap();
		files
			.write_path_replace("refs/heads/alias", b"ref: refs/heads/main\n")
			.await
			.unwrap();

		let error = transaction
			.publish_durable(commit, ReflogIntent::Skip)
			.await
			.unwrap_err();

		assert!(matches!(error, RepositoryError::ExistingHistory));
		assert_eq!(repository.refs().resolve_head().await.unwrap(), None);
		assert_eq!(
			repository.refs().resolve("refs/heads/main").await.unwrap(),
			None
		);
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn cancelled_merge_state_write_retains_the_history_lease() {
		let files = crate::GatedFileStore::new();
		let repository = Repository::<_, Sha256>::new(ObjectStore::new(files.shared_handle()));
		repository.init().await.unwrap();
		let commit = initial_commit(&repository).await;
		let mut mutation = Box::pin(repository.start_merge(commit, "Merge in progress\n"));
		let mut context = Context::from_waker(Waker::noop());
		assert!(matches!(
			mutation.as_mut().poll(&mut context),
			Poll::Pending
		));
		files.wait_until_blocked().await;

		drop(mutation);
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(files.exists("gitana-history.lock").await.unwrap());

		files.release();
		for _ in 0..50 {
			tokio::task::yield_now().await;
			if !files.exists("gitana-history.lock").await.unwrap() {
				break;
			}
		}
		assert_eq!(repository.merge_head().await.unwrap(), Some(commit));
		assert!(!files.exists("gitana-history.lock").await.unwrap());
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn cancelled_shallow_write_retains_the_history_lease() {
		let files = crate::GatedFileStore::new();
		let repository = Repository::<_, Sha256>::new(ObjectStore::new(files.shared_handle()));
		repository.init().await.unwrap();
		let commit = initial_commit(&repository).await;
		let commits = [commit];
		let mut mutation = Box::pin(repository.write_shallow(&commits));
		let mut context = Context::from_waker(Waker::noop());
		assert!(matches!(
			mutation.as_mut().poll(&mut context),
			Poll::Pending
		));
		files.wait_until_blocked().await;

		drop(mutation);
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(files.exists("gitana-history.lock").await.unwrap());

		files.release();
		for _ in 0..50 {
			tokio::task::yield_now().await;
			if !files.exists("gitana-history.lock").await.unwrap() {
				break;
			}
		}
		assert_eq!(repository.read_shallow().await.unwrap(), vec![commit]);
		assert!(!files.exists("gitana-history.lock").await.unwrap());
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn cancelled_reflog_append_retains_the_history_lease() {
		let files = crate::GatedFileStore::new();
		let repository = Repository::<_, Sha256>::new(ObjectStore::new(files.shared_handle()));
		repository.init().await.unwrap();
		let commit = initial_commit(&repository).await;
		let refs = repository.refs();
		let mut mutation = Box::pin(refs.append_reflog(
			"refs/heads/deleted",
			None,
			Some(commit),
			IDENTITY,
			"retained",
		));
		let mut context = Context::from_waker(Waker::noop());
		assert!(matches!(
			mutation.as_mut().poll(&mut context),
			Poll::Pending
		));
		files.wait_until_blocked().await;

		drop(mutation);
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(files.exists("gitana-history.lock").await.unwrap());

		files.release();
		for _ in 0..50 {
			tokio::task::yield_now().await;
			if !files.exists("gitana-history.lock").await.unwrap() {
				break;
			}
		}
		assert!(files.exists("logs/refs/heads/deleted").await.unwrap());
		assert!(!files.exists("gitana-history.lock").await.unwrap());
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn cancelled_publication_retains_the_history_lock_until_durable_completion() {
		let files = crate::GatedFileStore::new();
		let repository = Repository::<_, Sha256>::new(ObjectStore::new(files.shared_handle()));
		repository.init().await.unwrap();
		let tree = repository.write_tree(&[]).await.unwrap();
		let commit = repository
			.create_commit(tree, Vec::new(), IDENTITY, IDENTITY, "Initial commit\n")
			.await
			.unwrap();
		repository
			.durability_barrier_object_graph(commit, &[])
			.await
			.unwrap();
		let transaction = repository.lock_initial_commit_transaction().await.unwrap();
		let mut publish = Box::pin(transaction.publish_durable(commit, ReflogIntent::Skip));
		let mut context = Context::from_waker(Waker::noop());
		assert!(matches!(publish.as_mut().poll(&mut context), Poll::Pending));
		files.wait_until_blocked().await;

		drop(publish);
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(files.exists("gitana-history.lock").await.unwrap());
		assert!(matches!(
			repository
				.refs()
				.update_ref("refs/heads/other", commit, None, ReflogIntent::Skip)
				.await,
			Err(RepositoryError::HistoryLocked)
		));

		files.release();
		for _ in 0..50 {
			tokio::task::yield_now().await;
			if !files.exists("gitana-history.lock").await.unwrap() {
				break;
			}
		}
		assert_eq!(
			repository.refs().resolve_head().await.unwrap(),
			Some(commit)
		);
		assert!(!files.exists("gitana-history.lock").await.unwrap());
	}
}
