//! gta refuses a reftable repository (`git init --ref-format=reftable`) up front, naming it, and
//! leaves the repository exactly as git made it: gitana implements only the files ref backend. Without
//! the refusal, gta tripped over the decoy `refs/heads` file git leaves for older tools with an opaque
//! "Not a directory", and `gta symbolic-ref HEAD` printed the decoy `refs/heads/.invalid` as `HEAD`.
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use support::{git, git_supports_sha256, gta, unique_tmp};

#[tokio::test]
async fn gta_refuses_a_reftable_repository_and_leaves_it_alone() {
	let probe = unique_tmp("reftable-probe");
	let reftable = Command::new("git")
		.args(["init", "-q", "--ref-format=reftable"])
		.arg(&probe)
		.output()
		.is_ok_and(|out| out.status.success());
	let _ = std::fs::remove_dir_all(&probe);
	if !reftable {
		eprintln!("skipping: git without --ref-format=reftable (git 2.45 or later)");
		return;
	}

	let mut formats = vec!["sha1"];
	if git_supports_sha256() {
		formats.push("sha256");
	}
	for fmt in formats {
		let base = unique_tmp(&format!("reftable-{fmt}"));
		let work = base.join("repo");
		git(
			&base,
			&[
				"init",
				"-q",
				"--ref-format=reftable",
				&format!("--object-format={fmt}"),
				"repo",
			],
		);
		std::fs::write(work.join("a.txt"), "1\n").unwrap();
		git(&work, &["add", "a.txt"]);
		git(
			&work,
			&[
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@e",
				"commit",
				"-q",
				"-m",
				"init",
			],
		);
		git(&work, &["branch", "feature"]);
		let head = git(&work, &["rev-parse", "HEAD"]);
		let before = files_under(&work);

		let w = work.to_str().unwrap();
		let commands: &[&[&str]] = &[
			&["status"],
			&["log"],
			&["symbolic-ref", "HEAD"],
			&["rev-parse", &head],
			&["branch", "new"],
			&["update-ref", "refs/tags/x", &head],
			&["commit", "-m", "second"],
			&["prune"],
			&["worktree", "add", "../wt"],
		];
		for args in commands {
			let out = gta(&[&["-C", w][..], args].concat()).await;
			let stderr = String::from_utf8_lossy(&out.stderr);
			assert!(
				!out.status.success() && stderr.contains("extensions.refstorage = reftable"),
				"{fmt}: gta {args:?}: exit {:?}, stdout {:?}, stderr {stderr:?}",
				out.status.code(),
				String::from_utf8_lossy(&out.stdout)
			);
			assert!(
				files_under(&work) == before,
				"{fmt}: gta {args:?} left the repository as git made it"
			);
		}
		assert!(!base.join("wt").exists(), "{fmt}: no worktree was made");
		let _ = std::fs::remove_dir_all(&base);
	}
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
