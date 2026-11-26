#![cfg(unix)]
//! How status reports a submodule, given what a [`SubmoduleProbe`] says is inside it and the
//! ignore settings. The probe here is a fake; the expected output is what git 2.55 printed for the
//! same states (measured with a real submodule). The end-to-end comparison with git, through the
//! native probe, is in `gta`'s tests.

use std::error::Error;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_path::GitPath;
use gitana_repository::Repository;
use gitana_test_git::git_command;
use gitana_worktree::{
	IgnoreSubmodules, Status, SubmoduleProbe, SubmoduleReport, SubmoduleState, WorkTree,
	WorktreeError,
};

/// A repository whose one commit records a file `top` and the submodule `sub`, mounted as git
/// mounts it: `sub/.git` names `.git/modules/sub`, whose `HEAD` is the recorded commit. The module
/// holds nothing else, which is all status reads of it itself; the rest is the probe's.
struct Fixture<H: HashAlgorithm> {
	work: PathBuf,
	recorded: ObjectId<H>,
}

impl<H: HashAlgorithm> Fixture<H> {
	/// The fixture, or `None` when git can't make a repository in this object format.
	fn new(tag: &str) -> Option<Self> {
		use std::sync::atomic::AtomicU64;
		static SEQ: AtomicU64 = AtomicU64::new(0);
		let work = std::env::temp_dir().join(format!(
			"gitana-worktree-submodule-{tag}-{}-{}-{}",
			H::NAME,
			std::process::id(),
			SEQ.fetch_add(1, Ordering::Relaxed)
		));
		let _ = std::fs::remove_dir_all(&work);
		std::fs::create_dir_all(&work).unwrap();
		let fixture = Self {
			recorded: commit::<H>("recorded"),
			work,
		};
		let format = format!("--object-format={}", H::NAME);
		if !git_at(&fixture.work, &["init", "-q", &format])
			.status
			.success()
		{
			return None;
		}
		std::fs::write(fixture.work.join("top"), "top\n").unwrap();
		fixture.git(&["add", "top"]);
		fixture.record(fixture.recorded);
		fixture.git(&["commit", "-q", "-m", "init"]);
		std::fs::create_dir_all(fixture.work.join("sub")).unwrap();
		std::fs::write(
			fixture.work.join("sub/.git"),
			"gitdir: ../.git/modules/sub\n",
		)
		.unwrap();
		std::fs::create_dir_all(fixture.work.join(".git/modules/sub")).unwrap();
		fixture.check_out(fixture.recorded);
		Some(fixture)
	}

	fn git(&self, args: &[&str]) -> String {
		let out = git_at(&self.work, args);
		assert!(
			out.status.success(),
			"git {args:?} failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8(out.stdout).unwrap()
	}

	/// Stages `sub` at `commit`.
	fn record(&self, commit: ObjectId<H>) {
		let info = format!("160000,{commit},sub");
		self.git(&["update-index", "--add", "--cacheinfo", &info]);
	}

	/// Checks `commit` out in the submodule, as far as its `HEAD` says.
	fn check_out(&self, commit: ObjectId<H>) {
		std::fs::write(
			self.work.join(".git/modules/sub/HEAD"),
			format!("{commit}\n"),
		)
		.unwrap();
	}

	fn tree(&self) -> WorkTree<LocalFileStore, CapWorkDir, H> {
		let git_dir = self.work.join(".git");
		WorkTree::new(
			Repository::new(ObjectStore::<_, H>::new(LocalFileStore::from_dir(open(
				&git_dir,
			)))),
			CapWorkDir::from_dir(open(&self.work)),
			&git_dir,
		)
	}

	async fn status(
		&self,
		build: impl FnOnce(
			WorkTree<LocalFileStore, CapWorkDir, H>,
		) -> WorkTree<LocalFileStore, CapWorkDir, H>,
	) -> Status {
		build(self.tree()).status(None).await.unwrap()
	}
}

impl<H: HashAlgorithm> Drop for Fixture<H> {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.work);
	}
}

fn git_at(work: &std::path::Path, args: &[&str]) -> std::process::Output {
	git_command()
		.current_dir(work)
		.args([
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@example.com",
			"-c",
			"commit.gpgsign=false",
		])
		.args(args)
		.output()
		.expect("run git")
}

fn open(path: &std::path::Path) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap()
}

/// A commit id that names no commit, which a gitlink may record.
fn commit<H: HashAlgorithm>(name: &str) -> ObjectId<H> {
	ObjectId::compute(ObjectKind::Commit, name.as_bytes())
}

/// A probe that gives one answer, and notes what it's asked: the recorded commit and the flag.
struct Probe {
	answer: Result<Option<SubmoduleReport>, &'static str>,
	asked: Mutex<Vec<(String, Option<IgnoreSubmodules>)>>,
}

impl Probe {
	fn new(answer: Result<Option<SubmoduleReport>, &'static str>) -> Arc<Self> {
		Arc::new(Self {
			answer,
			asked: Mutex::new(Vec::new()),
		})
	}

	fn reporting(ignore: IgnoreSubmodules, modified: bool, untracked: bool) -> Arc<Self> {
		Self::new(Ok(Some(SubmoduleReport {
			ignore,
			new_commits: false,
			modified_content: modified,
			untracked_content: untracked,
		})))
	}

	/// A probe that finds new commits, as for a layout the work tree can't read itself.
	fn finding_new_commits(ignore: IgnoreSubmodules) -> Arc<Self> {
		Self::new(Ok(Some(SubmoduleReport {
			ignore,
			new_commits: true,
			..SubmoduleReport::default()
		})))
	}

	fn asked(&self) -> Vec<(String, Option<IgnoreSubmodules>)> {
		self.asked.lock().unwrap().clone()
	}
}

impl<H: HashAlgorithm> SubmoduleProbe<H> for Probe {
	fn report<'a>(
		&'a self,
		path: &'a GitPath,
		recorded: ObjectId<H>,
		flag: Option<IgnoreSubmodules>,
	) -> Pin<
		Box<
			dyn Future<Output = Result<Option<SubmoduleReport>, Box<dyn Error + Send + Sync>>>
				+ Send
				+ 'a,
		>,
	> {
		self.asked.lock().unwrap().push((recorded.to_hex(), flag));
		Box::pin(async move {
			assert_eq!(path.to_string(), "sub");
			self.answer.map_err(Into::into)
		})
	}
}

/// The submodule's state on its entry, which must be there.
fn state(status: &Status) -> SubmoduleState {
	status
		.changed
		.iter()
		.find(|entry| entry.path == "sub")
		.and_then(|entry| entry.submodule)
		.expect("sub is reported as a submodule")
}

const NEW_COMMITS: SubmoduleState = SubmoduleState {
	new_commits: true,
	modified_content: false,
	untracked_content: false,
};
const MODIFIED: SubmoduleState = SubmoduleState {
	new_commits: false,
	modified_content: true,
	untracked_content: false,
};
const UNTRACKED: SubmoduleState = SubmoduleState {
	new_commits: false,
	modified_content: false,
	untracked_content: true,
};

/// Runs `case` in both object formats.
macro_rules! both_formats {
	($name:ident, $case:ident) => {
		#[tokio::test]
		async fn $name() {
			$case::<Sha1>().await;
			$case::<Sha256>().await;
		}
	};
}

fn probed<H: HashAlgorithm>(
	probe: &Arc<Probe>,
) -> impl FnOnce(WorkTree<LocalFileStore, CapWorkDir, H>) -> WorkTree<LocalFileStore, CapWorkDir, H>
{
	let probe: Arc<dyn SubmoduleProbe<H>> = probe.clone();
	move |tree| tree.with_submodule_probe(probe)
}

async fn compared_by_its_commit_without_a_probe<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("commit") else {
		return;
	};
	assert_eq!(fixture.status(|tree| tree).await.porcelain_v1(), "");
	fixture.check_out(commit::<H>("other"));
	std::fs::write(fixture.work.join("top"), "changed\n").unwrap();
	let status = fixture.status(|tree| tree).await;
	assert_eq!(status.porcelain_v1(), " M sub\n M top\n");
	assert_eq!(state(&status), NEW_COMMITS);
	assert_eq!(status.changed[1].submodule, None, "top isn't a submodule");
}
both_formats!(
	a_submodule_is_compared_by_its_commit_without_a_probe,
	compared_by_its_commit_without_a_probe
);

async fn new_commits_the_probe_finds<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("probe-commits") else {
		return;
	};
	// The work tree itself sees the recorded commit checked out; the probe knows better.
	let probe = Probe::finding_new_commits(IgnoreSubmodules::None);
	let status = fixture.status(probed(&probe)).await;
	assert_eq!(status.porcelain_v1(), " M sub\n");
	assert_eq!(state(&status), NEW_COMMITS);
	assert_eq!(probe.asked(), [(fixture.recorded.to_hex(), None)]);
	let probe = Probe::finding_new_commits(IgnoreSubmodules::Dirty);
	assert_eq!(state(&fixture.status(probed(&probe)).await), NEW_COMMITS);
	let probe = Probe::finding_new_commits(IgnoreSubmodules::All);
	assert_eq!(fixture.status(probed(&probe)).await.porcelain_v1(), "");
}
both_formats!(
	a_submodule_with_new_commits_the_probe_finds_is_modified,
	new_commits_the_probe_finds
);

async fn content_the_probe_finds<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("content") else {
		return;
	};
	for (modified, untracked, expected) in [
		(true, false, MODIFIED),
		(false, true, UNTRACKED),
		(
			true,
			true,
			SubmoduleState {
				modified_content: true,
				untracked_content: true,
				..SubmoduleState::default()
			},
		),
	] {
		let probe = Probe::reporting(IgnoreSubmodules::None, modified, untracked);
		let status = fixture.status(probed(&probe)).await;
		assert_eq!(status.porcelain_v1(), " M sub\n");
		assert_eq!(state(&status), expected);
	}
	let probe = Probe::reporting(IgnoreSubmodules::None, false, false);
	assert_eq!(fixture.status(probed(&probe)).await.porcelain_v1(), "");
}
both_formats!(
	a_submodule_with_content_the_probe_finds_is_modified,
	content_the_probe_finds
);

async fn the_submodule_s_own_setting<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("setting") else {
		return;
	};
	use IgnoreSubmodules::{All, Dirty, Untracked};
	let probe = Probe::reporting(Untracked, true, true);
	let status = fixture.status(probed(&probe)).await;
	assert_eq!(status.porcelain_v1(), " M sub\n");
	assert_eq!(
		state(&status),
		MODIFIED,
		"untracked hides untracked content"
	);
	let probe = Probe::reporting(Untracked, false, true);
	assert_eq!(fixture.status(probed(&probe)).await.porcelain_v1(), "");
	let probe = Probe::reporting(Dirty, true, true);
	assert_eq!(
		fixture.status(probed(&probe)).await.porcelain_v1(),
		"",
		"dirty hides all content"
	);

	fixture.check_out(commit::<H>("other"));
	let probe = Probe::reporting(Dirty, true, true);
	let status = fixture.status(probed(&probe)).await;
	assert_eq!(status.porcelain_v1(), " M sub\n");
	assert_eq!(state(&status), NEW_COMMITS, "dirty leaves new commits");
	let probe = Probe::reporting(All, true, true);
	assert_eq!(
		fixture.status(probed(&probe)).await.porcelain_v1(),
		"",
		"all hides new commits too"
	);
}
both_formats!(
	the_submodule_s_own_setting_hides_what_it_says,
	the_submodule_s_own_setting
);

async fn a_deleted_or_replaced_mount<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("mount") else {
		return;
	};
	std::fs::remove_dir_all(fixture.work.join("sub")).unwrap();
	let status = fixture.status(|tree| tree).await;
	assert_eq!(status.porcelain_v1(), " D sub\n");
	assert_eq!(state(&status), SubmoduleState::default());
	let probe = Probe::reporting(IgnoreSubmodules::Dirty, false, false);
	assert_eq!(
		fixture.status(probed(&probe)).await.porcelain_v1(),
		" D sub\n"
	);
	let probe = Probe::reporting(IgnoreSubmodules::All, false, false);
	assert_eq!(fixture.status(probed(&probe)).await.porcelain_v1(), "");
	assert_eq!(
		probe.asked().len(),
		1,
		"asked for its setting, with no mount"
	);

	std::fs::write(fixture.work.join("sub"), "a file\n").unwrap();
	assert_eq!(fixture.status(|tree| tree).await.porcelain_v1(), " T sub\n");
	let probe = Probe::reporting(IgnoreSubmodules::All, false, false);
	assert_eq!(fixture.status(probed(&probe)).await.porcelain_v1(), "");
}
both_formats!(
	all_hides_a_deleted_or_replaced_mount,
	a_deleted_or_replaced_mount
);

async fn the_flag_outranks<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("flag") else {
		return;
	};
	let probe = Probe::reporting(IgnoreSubmodules::All, true, false);
	let status = fixture
		.status(|tree| probed(&probe)(tree).with_ignore_submodules(IgnoreSubmodules::None))
		.await;
	assert_eq!(status.porcelain_v1(), " M sub\n");
	assert_eq!(state(&status), MODIFIED);

	let probe = Probe::reporting(IgnoreSubmodules::None, true, true);
	let status = fixture
		.status(|tree| probed(&probe)(tree).with_ignore_submodules(IgnoreSubmodules::Untracked))
		.await;
	assert_eq!(state(&status), MODIFIED);

	// `dirty` still asks, about new commits, and passes the flag on so the probe can skip the
	// content; `all` leaves it nothing to add.
	fixture.check_out(commit::<H>("other"));
	let probe = Probe::reporting(IgnoreSubmodules::None, true, true);
	let status = fixture
		.status(|tree| probed(&probe)(tree).with_ignore_submodules(IgnoreSubmodules::Dirty))
		.await;
	assert_eq!(state(&status), NEW_COMMITS);
	assert_eq!(
		probe.asked(),
		[(fixture.recorded.to_hex(), Some(IgnoreSubmodules::Dirty))]
	);
	let probe = Probe::reporting(IgnoreSubmodules::None, true, true);
	let status = fixture
		.status(|tree| probed(&probe)(tree).with_ignore_submodules(IgnoreSubmodules::All))
		.await;
	assert_eq!(status.porcelain_v1(), "");
	assert_eq!(probe.asked(), []);

	// Nor is it asked about a deleted mount when the flag sets the rule.
	std::fs::remove_dir_all(fixture.work.join("sub")).unwrap();
	let status = fixture
		.status(|tree| probed(&probe)(tree).with_ignore_submodules(IgnoreSubmodules::None))
		.await;
	assert_eq!(status.porcelain_v1(), " D sub\n");
	assert_eq!(probe.asked(), []);
}
both_formats!(
	the_flag_outranks_the_submodule_s_own_setting,
	the_flag_outranks
);

async fn staged_gitlink_changes<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("staged") else {
		return;
	};
	// Moved to another commit, and staged.
	let other = commit::<H>("other");
	fixture.record(other);
	fixture.check_out(other);
	let status = fixture.status(|tree| tree).await;
	assert_eq!(status.porcelain_v1(), "M  sub\n");
	assert_eq!(state(&status), SubmoduleState::default());
	let probe = Probe::reporting(IgnoreSubmodules::All, true, true);
	assert_eq!(
		fixture.status(probed(&probe)).await.porcelain_v1(),
		"M  sub\n",
		"a setting in config never hides a staged change"
	);
	let all = |tree: WorkTree<_, _, H>| tree.with_ignore_submodules(IgnoreSubmodules::All);
	assert_eq!(fixture.status(all).await.porcelain_v1(), "");

	// Removed from the index: the mount is then an untracked folder, which the flag leaves.
	fixture.git(&["update-index", "--force-remove", "sub"]);
	let status = fixture.status(|tree| tree).await;
	assert_eq!(status.porcelain_v1(), "D  sub\n?? sub/\n");
	assert_eq!(state(&status), SubmoduleState::default());
	assert_eq!(fixture.status(all).await.porcelain_v1(), "?? sub/\n");

	// Replaced by a file in the index: the flag leaves that too.
	std::fs::remove_dir_all(fixture.work.join("sub")).unwrap();
	std::fs::write(fixture.work.join("sub"), "a file\n").unwrap();
	fixture.git(&["add", "sub"]);
	let status = fixture.status(all).await;
	assert_eq!(status.changed.len(), 1);
	assert_ne!(status.changed[0].index, ' ', "{}", status.porcelain_v1());
	assert_eq!(state(&status), SubmoduleState::default());
}
both_formats!(
	the_flag_s_all_hides_staged_gitlink_changes_too,
	staged_gitlink_changes
);

async fn trusted_gitlinks<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("trusted") else {
		return;
	};
	fixture.check_out(commit::<H>("other"));
	for flag in ["--skip-worktree", "--assume-unchanged"] {
		fixture.git(&["update-index", flag, "sub"]);
		let probe = Probe::reporting(IgnoreSubmodules::None, true, true);
		assert_eq!(
			fixture.status(probed(&probe)).await.porcelain_v1(),
			"",
			"{flag}"
		);
		assert_eq!(probe.asked(), [], "{flag}");
		fixture.git(&["update-index", &flag.replace("--", "--no-"), "sub"]);
	}
}
both_formats!(
	a_skip_worktree_or_assume_unchanged_gitlink_asks_nothing,
	trusted_gitlinks
);

async fn nothing_to_say<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("nothing") else {
		return;
	};
	let probe = Probe::new(Ok(None));
	assert_eq!(fixture.status(probed(&probe)).await.porcelain_v1(), "");
	fixture.check_out(commit::<H>("other"));
	let status = fixture.status(probed(&probe)).await;
	assert_eq!(status.porcelain_v1(), " M sub\n");
	assert_eq!(state(&status), NEW_COMMITS);
}
both_formats!(
	a_probe_with_nothing_to_say_leaves_the_commit_comparison,
	nothing_to_say
);

async fn a_failing_probe<H: HashAlgorithm>() {
	let Some(fixture) = Fixture::<H>::new("failing") else {
		return;
	};
	let probe: Arc<dyn SubmoduleProbe<H>> = Probe::new(Err("the module is broken"));
	let error = fixture
		.tree()
		.with_submodule_probe(probe)
		.status(None)
		.await
		.unwrap_err();
	let WorktreeError::Submodule { path, .. } = &error else {
		panic!("not a submodule error: {error}");
	};
	assert_eq!(path.to_string(), "sub");
	assert_eq!(
		error.to_string(),
		"couldn't read the status of submodule 'sub': the module is broken"
	);
}
both_formats!(
	a_failing_probe_fails_the_status_naming_the_submodule,
	a_failing_probe
);
