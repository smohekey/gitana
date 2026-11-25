#![cfg(unix)]
//! `gta rebase` with a submodule changed, against git 2.55 on an identical copy, in both object
//! formats. git's clean-tree gate ignores a submodule's changes; so does gta's, except a staged
//! gitlink change, which gta refuses up front where git's pick fails mid-rebase.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A superproject on `topic` (one commit past `base`) with the submodule `sub`, and `main` one
/// commit past `base` too, so `rebase main` replays `topic`'s commit.
const FIXTURE: &str = r#"
mkdir sub super
g -C sub init -q --object-format=$FORMAT; echo s > sub/f; g -C sub add -A; g -C sub commit -q -m s
g -C super init -q --object-format=$FORMAT; echo t > super/top; g -C super add -A; g -C super commit -q -m t
cd super; g submodule add -q ../sub sub; g commit -q -m add
g branch base; echo u > up; g add up; g commit -q -m up
g checkout -q -b topic base; echo w > work; g add work; g commit -q -m work
"#;

const PRELUDE: &str = r#"
set -e
g() { git -c user.name=T -c user.email=t@example.com -c commit.gpgsign=false -c init.defaultBranch=main -c protocol.file.allow=always "$@"; }
"#;

struct Fixture {
	base: tempfile::TempDir,
	format: &'static str,
}

impl Fixture {
	/// The fixture, with `change` made in the superproject.
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
			.env("GIT_AUTHOR_NAME", "T")
			.env("GIT_AUTHOR_EMAIL", "t@example.com")
			.env("GIT_COMMITTER_NAME", "T")
			.env("GIT_COMMITTER_EMAIL", "t@example.com")
			.env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
			.env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z");
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

	fn run(&self, program: &str, args: &[&str]) -> Output {
		let mut command = if program == "gta" {
			let mut command = Command::new(assert_cmd::cargo::cargo_bin("gta"));
			command.arg("-C").arg(self.superproject());
			command
		} else {
			let mut command = Command::new("git");
			command
				.args(["-c", "commit.gpgsign=false", "-C"])
				.arg(self.superproject());
			command
		};
		self.isolated(&mut command);
		command.args(args).output().unwrap()
	}

	fn head(&self) -> String {
		let out = self.run("git", &["rev-parse", "HEAD"]);
		String::from_utf8(out.stdout).unwrap().trim().to_owned()
	}

	fn in_progress(&self) -> bool {
		let git = self.superproject().join(".git");
		[
			"rebase-merge",
			"rebase-apply",
			"REBASE_HEAD_NAME",
			"REBASE_TODO",
		]
		.iter()
		.any(|name| git.join(name).exists())
	}
}

#[test]
fn rebases_over_a_submodule_s_unstaged_changes_as_git_does() {
	for format in ["sha1", "sha256"] {
		for (label, change) in [
			(
				"new commits",
				"echo x >> sub/f; g -C sub commit -q -am moved",
			),
			("modified content", "echo y >> sub/f"),
			("untracked content", "echo y > sub/new"),
			("mount deleted", "rm -rf sub"),
			("mount replaced by a file", "rm -rf sub; echo f > sub"),
		] {
			let label = format!("{label} ({format})");
			let git = Fixture::new(format, change);
			let out = git.run("git", &["rebase", "-q", "main"]);
			assert!(out.status.success(), "{label}: git refused: {out:?}");

			let gta = Fixture::new(format, change);
			let main = String::from_utf8(gta.run("git", &["rev-parse", "main"]).stdout).unwrap();
			let out = gta.run("gta", &["rebase", "main"]);
			assert!(out.status.success(), "{label}: gta refused: {out:?}");
			let parent = gta.run("git", &["rev-parse", "HEAD~1"]);
			assert_eq!(
				String::from_utf8(parent.stdout).unwrap(),
				main,
				"{label}: replayed onto main"
			);
			assert!(!gta.in_progress(), "{label}: finished");
		}
	}
}

#[test]
fn refuses_a_staged_gitlink_change_before_anything_moves() {
	let staged = "echo x >> sub/f; g -C sub commit -q -am moved; g add sub";
	for format in ["sha1", "sha256"] {
		// git's gate lets it through, and its pick then fails with the rebase left in progress.
		let git = Fixture::new(format, staged);
		let out = git.run("git", &["rebase", "-q", "main"]);
		assert!(!out.status.success(), "{format}: git rebased");
		assert!(
			String::from_utf8_lossy(&out.stderr)
				.contains("your local changes would be overwritten by rebase"),
			"{format}: {out:?}"
		);
		assert!(
			git.in_progress(),
			"{format}: git's rebase is left in progress"
		);

		// gta refuses before starting: nothing moves, and nothing is left to continue or abort.
		let gta = Fixture::new(format, staged);
		let before = gta.head();
		let out = gta.run("gta", &["rebase", "main"]);
		assert!(!out.status.success(), "{format}: gta rebased");
		assert_eq!(gta.head(), before, "{format}: HEAD hasn't moved");
		assert!(!gta.in_progress(), "{format}: no rebase in progress");
		let status = gta.run("git", &["status", "--porcelain"]);
		assert_eq!(
			String::from_utf8(status.stdout).unwrap(),
			"M  sub\n",
			"{format}: the staged change is kept"
		);
	}
}
