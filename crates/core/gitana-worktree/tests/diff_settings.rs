#![cfg(unix)]
//! What `status` refuses before it computes anything, in both object formats: a bad or valueless
//! `diff.ignoreSubmodules`, wherever git's status reads one (probed vs git 2.55), unless the caller
//! asked for `status_without_diff_check`; and a sparse index, always.

use std::path::Path;
use std::process::Command;

use gitana_config::GitConfig;
use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::{HashAlgorithm, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_worktree::{WorkTree, WorktreeError};

fn git(dir: &Path, args: &[&str]) {
	let out = Command::new("git")
		.current_dir(dir)
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.args([
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@example.com",
			"-c",
			"commit.gpgsign=false",
			"-c",
			"init.defaultBranch=main",
		])
		.args(args)
		.output()
		.unwrap();
	assert!(
		out.status.success(),
		"git {args:?}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// A committed repository in `format`, with files in two folders.
fn repository(format: &str) -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	git(
		dir.path(),
		&["init", "-q", &format!("--object-format={format}")],
	);
	for folder in ["one", "two"] {
		std::fs::create_dir_all(dir.path().join(folder)).unwrap();
		std::fs::write(dir.path().join(folder).join("f"), folder).unwrap();
	}
	git(dir.path(), &["add", "-A"]);
	git(dir.path(), &["commit", "-q", "-m", "init"]);
	dir
}

fn open(path: &Path) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap()
}

fn work_tree<H: HashAlgorithm>(
	dir: &Path,
	effective: Option<GitConfig>,
) -> WorkTree<LocalFileStore, CapWorkDir, H> {
	let git_dir = dir.join(".git");
	let mut repository = Repository::new(ObjectStore::<_, H>::new(LocalFileStore::from_dir(open(
		&git_dir,
	))));
	if let Some(config) = effective {
		repository.set_effective_config(config);
	}
	WorkTree::new(repository, CapWorkDir::from_dir(open(dir)), &git_dir)
}

fn append(path: &Path, text: &str) {
	let mut config = std::fs::read_to_string(path).unwrap_or_default();
	config.push_str(text);
	std::fs::write(path, config).unwrap();
}

/// What `status` says of a repository whose config files `prepare` changed, with `effective` the
/// config the frontend installed, if any: the refusal's text, or `None` if it computed one.
async fn refusal<H: HashAlgorithm>(
	format: &str,
	prepare: impl Fn(&Path),
	effective: Option<GitConfig>,
) -> (Option<String>, Option<String>) {
	let dir = repository(format);
	prepare(dir.path());
	let tree = work_tree::<H>(dir.path(), effective);
	let checked = tree.status(None).await.err().map(|error| error.to_string());
	let unchecked = tree
		.status_without_diff_check(None)
		.await
		.err()
		.map(|error| error.to_string());
	(checked, unchecked)
}

/// A case: its label, what it does to the repository's config files, the config the frontend
/// installs (if any), and the refusal `status` gives (if any).
type Case = (
	&'static str,
	Box<dyn Fn(&Path)>,
	Option<GitConfig>,
	Option<&'static str>,
);

const BAD: &str = "bad --ignore-submodules argument: bogus";
const MISSING: &str = "missing value for 'diff.ignoresubmodules'";

async fn refuses_a_bad_setting_wherever_git_reads_one<H: HashAlgorithm>(format: &str) {
	let local = |text: &'static str| move |dir: &Path| append(&dir.join(".git/config"), text);
	let parse = |text: &str| GitConfig::parse(text).unwrap();
	let cases: Vec<Case> = vec![
		("no setting", Box::new(|_: &Path| {}), None, None),
		(
			"a valid one",
			Box::new(local("[diff]\n\tignoreSubmodules = dirty\n")),
			None,
			None,
		),
		(
			"a bad one",
			Box::new(local("[diff]\n\tignoreSubmodules = bogus\n")),
			None,
			Some(BAD),
		),
		(
			"one with no value",
			Box::new(local("[diff]\n\tignoreSubmodules\n")),
			None,
			Some(MISSING),
		),
		(
			"an empty one, which is a bad one",
			Box::new(local("[diff]\n\tignoreSubmodules =\n")),
			None,
			Some("bad --ignore-submodules argument: "),
		),
		(
			"a bad one a valid one overrides",
			Box::new(local(
				"[diff]\n\tignoreSubmodules = bogus\n\tignoreSubmodules = none\n",
			)),
			None,
			Some(BAD),
		),
		(
			"git's case-sensitive spelling",
			Box::new(local("[diff]\n\tignoreSubmodules = None\n")),
			None,
			Some("bad --ignore-submodules argument: None"),
		),
		(
			"a bad one in config.worktree",
			Box::new(|dir: &Path| {
				append(
					&dir.join(".git/config"),
					"[extensions]\n\tworktreeConfig = true\n",
				);
				append(
					&dir.join(".git/config.worktree"),
					"[diff]\n\tignoreSubmodules = bogus\n",
				);
			}),
			None,
			Some(BAD),
		),
		(
			"config.worktree without extensions.worktreeConfig, which git doesn't read",
			Box::new(|dir: &Path| {
				append(
					&dir.join(".git/config.worktree"),
					"[diff]\n\tignoreSubmodules = bogus\n",
				);
			}),
			None,
			None,
		),
		(
			"a bad one in the installed config",
			Box::new(|_: &Path| {}),
			Some(parse("[diff]\n\tignoreSubmodules = bogus\n")),
			Some(BAD),
		),
		(
			"a bad global one a valid local one overrides, in the installed config",
			Box::new(|_: &Path| {}),
			Some(GitConfig::from_sources(vec![
				gitana_config::GitConfigSource::parse("[diff]\n\tignoreSubmodules = bogus\n").unwrap(),
				gitana_config::GitConfigSource::parse("[diff]\n\tignoreSubmodules = none\n").unwrap(),
			])),
			Some(BAD),
		),
		(
			"no config file at all",
			Box::new(|dir: &Path| std::fs::remove_file(dir.join(".git/config")).unwrap()),
			None,
			None,
		),
	];
	for (label, prepare, effective, expected) in cases {
		let (checked, unchecked) = refusal::<H>(format, prepare, effective).await;
		assert_eq!(checked.as_deref(), expected, "{label} ({format}): status");
		assert_eq!(
			unchecked, None,
			"{label} ({format}): status_without_diff_check"
		);
	}
}

#[tokio::test]
async fn refuses_a_bad_setting_wherever_git_reads_one_sha1() {
	refuses_a_bad_setting_wherever_git_reads_one::<Sha1>("sha1").await;
}

#[tokio::test]
async fn refuses_a_bad_setting_wherever_git_reads_one_sha256() {
	refuses_a_bad_setting_wherever_git_reads_one::<Sha256>("sha256").await;
}

async fn refuses_a_sparse_index<H: HashAlgorithm>(format: &str) {
	let dir = repository(format);
	git(
		dir.path(),
		&["sparse-checkout", "set", "--cone", "--sparse-index", "one"],
	);
	let tree = work_tree::<H>(dir.path(), None);
	assert!(tree.is_sparse_index().await.unwrap(), "{format}");
	assert!(
		matches!(tree.status(None).await, Err(WorktreeError::SparseIndex)),
		"{format}"
	);
	assert!(
		matches!(
			tree.status_without_diff_check(None).await,
			Err(WorktreeError::SparseIndex)
		),
		"{format}"
	);
	// A sparse checkout without a sparse index is read as before.
	git(
		dir.path(),
		&["sparse-checkout", "set", "--no-sparse-index", "one"],
	);
	let tree = work_tree::<H>(dir.path(), None);
	assert!(!tree.is_sparse_index().await.unwrap(), "{format}");
	let status = tree.status(None).await.unwrap();
	assert!(
		status.changed.is_empty() && status.untracked.is_empty(),
		"{format}: {status:?}"
	);
}

#[tokio::test]
async fn refuses_a_sparse_index_sha1() {
	refuses_a_sparse_index::<Sha1>("sha1").await;
}

#[tokio::test]
async fn refuses_a_sparse_index_sha256() {
	refuses_a_sparse_index::<Sha256>("sha256").await;
}
