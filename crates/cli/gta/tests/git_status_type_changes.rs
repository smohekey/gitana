#![cfg(unix)]
//! `gta status` on type changes, against `git status --porcelain` on the same repository, in both
//! object formats: `T` for another kind of object at a tracked path, staged or not; ` D` for a file
//! whose path is now a plain folder, which isn't listed as untracked; and `ls-files -m` and `-d`
//! unchanged by any of it, as git's are.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A repository with a file, a symlink to it, an executable, a folder, an ignored pattern and a
/// submodule `sub`, all committed.
const FIXTURE: &str = r#"
g init -q --object-format=$FORMAT sub-origin
(cd sub-origin && echo s > f && g add -A && g commit -q -m s)
g init -q --object-format=$FORMAT r
cd r
echo '*.log' > .gitignore; echo f > file; ln -s file link; echo x > exe; chmod +x exe
mkdir dir; echo d > dir/f; echo o > other
g add -A; g commit -q -m base
g submodule add -q ../sub-origin sub; g commit -q -m sub
"#;

const PRELUDE: &str = r#"
set -e
g() { git -c user.name=T -c user.email=t@example.com -c commit.gpgsign=false -c init.defaultBranch=main -c protocol.file.allow=always "$@"; }
"#;

/// A repository with a commit, made at `$1` in the repository's own object format.
const NESTED: &str = r#"nested() { g init -q --object-format=$FORMAT "$1"; (cd "$1" && echo z > x && g add -A && g commit -q -m n); }
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
		fixture.sh(&fixture.repository(), &format!("{NESTED}{change}"));
		fixture
	}

	fn home(&self) -> PathBuf {
		self.base.path().join("home")
	}

	fn repository(&self) -> PathBuf {
		self.base.path().join("r")
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

	fn run(&self, program: &str, args: &[&str]) -> Output {
		let mut command = if program == "gta" {
			Command::new(assert_cmd::cargo::cargo_bin("gta"))
		} else {
			Command::new("git")
		};
		self.isolated(&mut command);
		command
			.current_dir(self.repository())
			.args(args)
			.output()
			.unwrap()
	}

	/// gta's status, `ls-files -m` and `ls-files -d` must print git's.
	fn check(&self, label: &str) {
		let label = format!("{label} ({})", self.format);
		for (git_args, gta_args) in [
			(&["status", "--porcelain"][..], &["status"][..]),
			(&["ls-files", "-m"][..], &["ls-files", "-m"][..]),
			(&["ls-files", "-d"][..], &["ls-files", "-d"][..]),
		] {
			let git = self.run("git", git_args);
			assert!(git.status.success(), "{label}: git {git_args:?}: {git:?}");
			let gta = self.run("gta", gta_args);
			assert!(gta.status.success(), "{label}: gta {gta_args:?}: {gta:?}");
			assert_eq!(
				String::from_utf8_lossy(&gta.stdout),
				String::from_utf8_lossy(&git.stdout),
				"{label}: {gta_args:?}"
			);
		}
	}
}

fn each(cases: &[(&str, &str)]) {
	for format in ["sha1", "sha256"] {
		for (label, change) in cases {
			Fixture::new(format, change).check(label);
		}
	}
}

#[test]
fn reports_an_unstaged_type_change_as_git_does() {
	each(&[
		("a file replaced by a symlink", "rm file; ln -s link file"),
		("a symlink replaced by a file", "rm link; echo y > link"),
		("only the exec bit, which is no type change", "chmod -x exe"),
		("a gitlink replaced by a file", "rm -rf sub; echo q > sub"),
		(
			"a file replaced by a repository with a commit",
			"rm file; nested file",
		),
		(
			"a symlink replaced by a repository with a commit",
			"rm link; nested link",
		),
		(
			"a symlink placeholder edited, with core.symlinks=false",
			"g config core.symlinks false; rm link; echo other > link",
		),
	]);
}

#[test]
fn reports_a_file_replaced_by_a_folder_as_deleted_as_git_does() {
	each(&[
		("an empty folder", "rm file; mkdir file"),
		(
			"a folder holding only ignored files",
			"rm file; mkdir file; echo l > file/a.log",
		),
		(
			"a folder holding a file",
			"rm file; mkdir file; echo z > file/x",
		),
		(
			"a repository with no commit yet",
			"rm file; g init -q --object-format=$FORMAT file; echo z > file/x",
		),
		(
			"a symlink replaced by a folder",
			"rm link; mkdir link; echo z > link/x",
		),
		("a folder replaced by a file", "rm -rf dir; echo z > dir"),
	]);
}

#[test]
fn reports_a_staged_type_change_as_git_does() {
	each(&[
		(
			"a file replaced by a symlink",
			"rm file; ln -s link file; g add file",
		),
		(
			"a symlink replaced by a file",
			"rm link; echo y > link; g add link",
		),
		(
			"staged, then changed back to its kind",
			"rm file; ln -s link file; g add file; rm file; echo f > file",
		),
		(
			"staged, then its target changed",
			"rm file; ln -s link file; g add file; rm file; ln -s dir file",
		),
		(
			"a gitlink replaced by a file",
			"g rm -q --cached sub; rm -rf sub; echo q > sub; g add sub",
		),
		(
			"a file replaced by a gitlink",
			"g rm -q --cached file; rm file; nested file; g add file",
		),
		(
			"a gitlink replaced by a symlink",
			"g rm -q --cached sub; rm -rf sub; ln -s file sub; g add sub",
		),
		(
			"a file replaced by a folder",
			"rm file; mkdir file; echo z > file/x; g add -A",
		),
	]);
}
