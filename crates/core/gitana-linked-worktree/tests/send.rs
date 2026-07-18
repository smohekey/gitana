//! Compile-time guard: the five public worktree-lifecycle futures must stay `Send`.
//!
//! A consumer (the intended external one, Code Henge) drives these on a multi-threaded runtime, where
//! `tokio::spawn` requires `Send`. No in-tree consumer forces this — `gta`/`gta-mcp` use current-thread
//! runtimes — so this test *is* the regression guard: it fails to compile if any transitive dependency
//! future loses `Send` (e.g. a boxed `dyn Future` drops its `+ Send`, or a `FileStore` method its bound).
#![cfg(all(unix, not(target_arch = "wasm32")))]

use std::path::{Path, PathBuf};

use gitana_linked_worktree::{
	BranchName, CheckoutTarget, CreateRequest, RemoveRequest, RepositoryId, WorktreeContext,
	WorktreeQuery, create, enumerate, inspect, remove, status,
};

fn assert_send<T: Send>(_: &T) {}

/// Construct each of the five public futures and assert each is `Send`. The futures are **never awaited**
/// — an `async fn` future does no work until polled — so the dummy paths need not exist; only the future
/// *types* matter. A regression in any transitive dependency's `Send`-ness breaks this crate's build here.
#[test]
fn public_op_futures_are_send() {
	let repo = RepositoryId::at_common_dir(PathBuf::from("/nonexistent/repo/.git")).unwrap();
	let destination = PathBuf::from("/nonexistent/wt");

	let create_request = CreateRequest {
		repo: repo.clone(),
		destination: destination.clone(),
		target: CheckoutTarget::Orphan {
			name: BranchName::new("wt"),
		},
	};
	let remove_request = RemoveRequest {
		repo: repo.clone(),
		destination: destination.clone(),
		expected_branch: None,
	};
	let query = WorktreeQuery {
		repo: repo.clone(),
		destination: destination.clone(),
		expected_branch: None,
		start: None,
		with_status: false,
	};
	let context = WorktreeContext::new(repo.clone());

	assert_send(&create(&create_request, None));
	assert_send(&remove(&remove_request));
	assert_send(&inspect(&query));
	assert_send(&enumerate(&context));
	assert_send(&status(&repo, Path::new("/nonexistent/wt")));
}
