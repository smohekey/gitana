//! The component's status looking inside submodules through the host's `submodules` capability,
//! against `git status --porcelain` on the same repository, in both object formats.

#![cfg(all(unix, not(target_arch = "wasm32")))]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Result, anyhow};
use gitana_config_native::Environment;
use gitana_repo_host::exports::gitana::repo::porcelain::{SubmoduleState, WorktreeStatus};
use gitana_repo_host::gitana::repo::submodule_types::IgnoreSubmodules;
use gitana_repo_host::{
	grant_dir, grant_submodules, instantiate_component, store, submodule_warnings,
};

use self::support::{shared, wit_path_bytes};

const PRELUDE: &str = r#"
set -e
g() { git -c user.name=T -c user.email=t@example.com -c commit.gpgsign=false -c init.defaultBranch=main -c protocol.file.allow=always "$@"; }
"#;

/// A superproject with the submodule `sub` (a tracked `f`), committed, as `git submodule add` makes
/// it.
const FIXTURE: &str = r#"
mkdir sub super
g -C sub init -q --object-format=$FORMAT; echo s > sub/f; g -C sub add -A; g -C sub commit -q -m s
g -C super init -q --object-format=$FORMAT; echo t > super/top; g -C super add -A; g -C super commit -q -m t
g -C super submodule add -q ../sub sub; g -C super commit -q -m 'add sub'
"#;

struct Fixture {
	base: tempfile::TempDir,
	format: &'static str,
}

impl Fixture {
	fn new(format: &'static str, change: &str) -> Self {
		let fixture = Self {
			base: tempfile::tempdir().unwrap(),
			format,
		};
		std::fs::create_dir_all(fixture.home()).unwrap();
		fixture.sh(fixture.base.path(), FIXTURE);
		fixture.sh(&fixture.superproject(), change);
		fixture
	}

	fn home(&self) -> PathBuf {
		self.base.path().join("home")
	}

	fn superproject(&self) -> PathBuf {
		self.base.path().join("super")
	}

	fn isolated(&self, command: &mut Command) {
		command
			.env("FORMAT", self.format)
			.env("HOME", self.home())
			.env("XDG_CONFIG_HOME", self.home())
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_ATTR_NOSYSTEM", "1");
	}

	fn sh(&self, dir: &Path, script: &str) {
		let mut command = Command::new("sh");
		self.isolated(&mut command);
		let out = command
			.current_dir(dir)
			.arg("-c")
			.arg(format!("{PRELUDE}{script}"))
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"{script}\n{}",
			String::from_utf8_lossy(&out.stderr)
		);
	}

	fn git_status(&self, args: &[&str]) -> Output {
		let mut command = Command::new("git");
		self.isolated(&mut command);
		command
			.current_dir(self.superproject())
			.args(["status", "--porcelain"])
			.args(args)
			.output()
			.unwrap()
	}

	fn environment(&self) -> Environment {
		Environment {
			home: Some(self.home()),
			xdg_config_home: Some(self.home()),
			config_global: Some(PathBuf::from("/dev/null")),
			config_nosystem: true,
			attr_nosystem: true,
		}
	}

	/// The component's status of the superproject, with the host's `submodules` capability granted
	/// when `granted`, and the warnings the host kept.
	async fn component_status(
		&self,
		granted: bool,
		flag: Option<IgnoreSubmodules>,
	) -> Result<(Result<WorktreeStatus, String>, Vec<String>)> {
		let superproject = self.superproject();
		let git_dir = superproject.join(".git");
		let run = async {
			let (engine, component) = shared();
			let mut store = store(engine);
			if granted {
				grant_submodules(&mut store, &superproject);
			}
			let repo = instantiate_component(engine, &mut store, component).await?;
			let git = grant_dir(&mut store, &git_dir)?;
			let common = grant_dir(&mut store, &git_dir)?;
			let work = grant_dir(&mut store, &superproject)?;
			let porcelain = repo.gitana_repo_porcelain().repository();
			let handle = porcelain
				.call_open_worktree(&mut store, git, common, work)
				.await?
				.map_err(|error| anyhow!("open-worktree: {error:?}"))?;
			let status = porcelain
				.call_status(&mut store, handle, flag)
				.await?
				.map_err(|error| format!("{error:?}"));
			Ok((status, submodule_warnings(&store)))
		};
		gitana_config_native::with_environment(self.environment(), run).await
	}
}

/// The component's status, printed as `git status --porcelain` prints it.
fn porcelain(status: &WorktreeStatus) -> String {
	let mut out = String::new();
	for entry in &status.changed {
		let path = String::from_utf8_lossy(wit_path_bytes(&entry.path));
		out.push_str(&format!("{}{} {path}\n", entry.index, entry.worktree));
	}
	for entry in &status.untracked {
		let path = String::from_utf8_lossy(wit_path_bytes(&entry.path));
		let slash = if entry.directory { "/" } else { "" };
		out.push_str(&format!("?? {path}{slash}\n"));
	}
	out
}

/// A submodule state's flags: new commits, modified content, untracked content.
fn flags(state: &SubmoduleState) -> (bool, bool, bool) {
	(
		state.new_commits,
		state.modified_content,
		state.untracked_content,
	)
}

#[tokio::test]
async fn reports_a_dirty_submodule_as_git_does() -> Result<()> {
	for format in ["sha1", "sha256"] {
		let fixture = Fixture::new(format, "echo y >> sub/f");
		let git = fixture.git_status(&[]);
		let (status, warnings) = fixture.component_status(true, None).await?;
		let status = status.map_err(|error| anyhow!("{format}: {error}"))?;
		assert_eq!(
			porcelain(&status),
			String::from_utf8_lossy(&git.stdout),
			"{format}"
		);
		assert_eq!(porcelain(&status), " M sub\n", "{format}");
		assert_eq!(
			status.changed[0].submodule.as_ref().map(flags),
			Some((false, true, false)),
			"{format}: modified content"
		);
		assert!(warnings.is_empty(), "{format}: {warnings:?}");

		// Without the grant, the submodule is compared by its checked-out commit alone.
		let (status, _) = fixture.component_status(false, None).await?;
		assert_eq!(
			porcelain(&status.map_err(|error| anyhow!("{format}: {error}"))?),
			"",
			"{format}: ungranted"
		);
	}
	Ok(())
}

#[tokio::test]
async fn honours_the_flag_as_git_does() -> Result<()> {
	for format in ["sha1", "sha256"] {
		// The flag outranks the submodule's own setting.
		let fixture = Fixture::new(
			format,
			"g config -f .gitmodules submodule.sub.ignore all; g commit -q -am all; echo y >> sub/f",
		);
		let git = fixture.git_status(&["--ignore-submodules=none"]);
		let (status, _) = fixture
			.component_status(true, Some(IgnoreSubmodules::None))
			.await?;
		let status = status.map_err(|error| anyhow!("{format}: {error}"))?;
		assert_eq!(
			porcelain(&status),
			String::from_utf8_lossy(&git.stdout),
			"{format}: flag none"
		);
		assert_eq!(porcelain(&status), " M sub\n", "{format}: flag none");

		// `all` hides a staged gitlink change too.
		let fixture = Fixture::new(
			format,
			"echo x >> sub/f; g -C sub commit -q -am moved; g add sub",
		);
		let git = fixture.git_status(&["--ignore-submodules=all"]);
		let (status, _) = fixture
			.component_status(true, Some(IgnoreSubmodules::All))
			.await?;
		let status = status.map_err(|error| anyhow!("{format}: {error}"))?;
		assert_eq!(
			porcelain(&status),
			String::from_utf8_lossy(&git.stdout),
			"{format}: flag all"
		);
		assert_eq!(porcelain(&status), "", "{format}: flag all");
	}
	Ok(())
}

#[tokio::test]
async fn keeps_git_s_warnings_and_fails_where_git_fails() -> Result<()> {
	for format in ["sha1", "sha256"] {
		let fixture = Fixture::new(
			format,
			r#"printf '[submodule "sub"]\n\tpath = sub\n\turl = ../sub\n\tignore = bogus\n' > .gitmodules"#,
		);
		let (status, warnings) = fixture.component_status(true, None).await?;
		status.map_err(|error| anyhow!("{format}: {error}"))?;
		assert_eq!(
			warnings,
			["Invalid parameter 'bogus' for config option 'submodule.sub.ignore'"],
			"{format}"
		);

		let fixture = Fixture::new(format, "echo 'gitdir: ../nowhere' > sub/.git");
		assert_eq!(fixture.git_status(&[]).status.code(), Some(128), "{format}");
		let (status, _) = fixture.component_status(true, None).await?;
		let error = status.expect_err("a broken submodule fails the status");
		assert!(error.contains("submodule 'sub'"), "{format}: {error}");
	}
	Ok(())
}

#[tokio::test]
async fn refuses_a_bad_diff_ignore_submodules_as_git_does() -> Result<()> {
	// git's status refuses a bad `diff.ignoreSubmodules`, with or without the flag, and so does the
	// component's, the submodule's own setting included (probed vs git 2.55).
	for format in ["sha1", "sha256"] {
		for (place, change) in [
			("the superproject", "g config diff.ignoreSubmodules bogus"),
			(
				"the submodule",
				"g -C sub config diff.ignoreSubmodules bogus",
			),
		] {
			let fixture = Fixture::new(format, change);
			assert_eq!(fixture.git_status(&[]).status.code(), Some(128), "{format}");
			let (status, _) = fixture.component_status(true, None).await?;
			let error = status.expect_err("a bad setting fails the status");
			assert!(
				error.contains("bad --ignore-submodules argument: bogus"),
				"{place} ({format}): {error}"
			);
		}
		let fixture = Fixture::new(format, "g config diff.ignoreSubmodules bogus");
		let (status, _) = fixture
			.component_status(true, Some(IgnoreSubmodules::None))
			.await?;
		assert!(status.is_err(), "{format}: the flag doesn't excuse it");
	}
	Ok(())
}
