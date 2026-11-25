//! gitana implements only the files ref backend, so every entry point refuses a reftable repository
//! (`git init --ref-format=reftable`) up front, naming it, and leaves the repository as git made it.
//! Without the refusal, `create`, `inspect` and `status` tripped over the decoy `refs/heads` file git
//! leaves for older tools, with an opaque "Not a directory".
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gitana_linked_worktree::{
	CheckoutTarget, CreateError, CreateRequest, LinkedWorktreeError, RelocateError, RelocateRequest,
	RemoveError, RemovePolicy, RemoveRequest, Setup, WorktreeObjectId, WorktreeQuery, create,
	enumerate, inspect, relocate, remove, status,
};

use self::common::{commit_file, ctx_at, formats, git, git_ok, rid_at, unique_tmp};

#[tokio::test]
async fn every_entry_point_refuses_a_reftable_repository() {
	let probe = unique_tmp("reftable-probe");
	if !git_ok(&[
		"init",
		"-q",
		"--ref-format=reftable",
		probe.to_str().unwrap(),
	]) {
		eprintln!("skipping: git without --ref-format=reftable (git 2.45 or later)");
		return;
	}
	let _ = std::fs::remove_dir_all(&probe);

	for (fmt, kind) in formats() {
		let base = unique_tmp(&format!("reftable-{fmt}"));
		let work = base.join("repo");
		let w = work.to_str().unwrap();
		git(&[
			"init",
			"-q",
			"-b",
			"main",
			"--ref-format=reftable",
			&format!("--object-format={fmt}"),
			w,
		]);
		git(&["-C", w, "config", "user.name", "T"]);
		git(&["-C", w, "config", "user.email", "t@e"]);
		git(&["-C", w, "config", "commit.gpgsign", "false"]);
		let head = commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-q",
			"--detach",
			wt.to_str().unwrap(),
		]);
		let before = files_under(&base);

		let repo = rid_at(&work);
		let start = WorktreeObjectId::parse(kind, &head).unwrap();
		let created = create(
			&CreateRequest {
				repo: repo.clone(),
				destination: base.join("new"),
				target: CheckoutTarget::Detached { start },
				committer: None,
				reflog_start: None,
				worktree_config: None,
				sparse_checkout: None,
			},
			&Setup::default(),
			&Setup::default(),
		)
		.await;
		match created {
			Err(CreateError::Failed(error)) => names_reftable(fmt, "create", &error),
			other => panic!("{fmt}: create: got {other:?}"),
		}

		let query = WorktreeQuery {
			repo: repo.clone(),
			destination: wt.clone(),
			expected_branch: None,
			start: None,
			with_status: false,
		};
		names_reftable(
			fmt,
			"inspect",
			&inspect(&query, &Setup::default()).await.unwrap_err(),
		);
		names_reftable(
			fmt,
			"status",
			&status(&repo, &wt, &Setup::default()).await.unwrap_err(),
		);
		names_reftable(
			fmt,
			"enumerate",
			&enumerate(&ctx_at(&work)).await.unwrap_err(),
		);

		let removed = remove(
			&RemoveRequest {
				repo: repo.clone(),
				destination: wt.clone(),
				expected_branch: None,
				policy: RemovePolicy::Conservative,
			},
			&Setup::default(),
		)
		.await;
		match removed {
			Err(RemoveError::Failed(error)) => names_reftable(fmt, "remove", &error),
			other => panic!("{fmt}: remove: got {other:?}"),
		}

		let relocated = relocate(&RelocateRequest {
			repo,
			from: wt.clone(),
			to: base.join("moved"),
			expected_branch: None,
			force: 0,
		})
		.await;
		match relocated {
			Err(RelocateError::Failed(error)) => names_reftable(fmt, "relocate", &error),
			other => panic!("{fmt}: relocate: got {other:?}"),
		}

		assert!(
			files_under(&base) == before,
			"{fmt}: the repository and its worktree are left as git made them"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

/// `error` is the unsupported-format refusal, and names reftable.
fn names_reftable(fmt: &str, what: &str, error: &LinkedWorktreeError) {
	assert!(
		matches!(error, LinkedWorktreeError::UnsupportedObjectFormat(message)
			if message.contains("extensions.refstorage = reftable")),
		"{fmt}: {what}: got {error:?}"
	);
}

/// Every file, directory and symlink under `root`, with each file's bytes and each symlink's target.
fn files_under(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
	let mut found = BTreeMap::new();
	let mut pending = vec![root.to_path_buf()];
	while let Some(directory) = pending.pop() {
		for entry in std::fs::read_dir(&directory).unwrap() {
			let path = entry.unwrap().path();
			let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
			let relative = path.strip_prefix(root).unwrap().to_path_buf();
			if kind.is_dir() {
				found.insert(relative, None);
				pending.push(path);
			} else if kind.is_symlink() {
				let target = std::fs::read_link(&path).unwrap();
				found.insert(relative, Some(target.into_os_string().into_encoded_bytes()));
			} else {
				found.insert(relative, Some(std::fs::read(&path).unwrap()));
			}
		}
	}
	found
}
