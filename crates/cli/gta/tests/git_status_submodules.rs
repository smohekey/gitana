#![cfg(unix)]
//! `gta status` looking inside submodules, against `git status --porcelain` on the same repository,
//! in both object formats: the states, `--ignore-submodules[=<when>]`, git's warnings, and the
//! settings git refuses. (The probe's full matrix of states, settings and layouts is in
//! `gitana-submodule-probe`'s tests; these prove the command line on top of it.)

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A superproject with the submodule `sub` (a tracked `f`, and a nested submodule `inner`), as
/// `git submodule add` and `update --init --recursive` make it.
const FIXTURE: &str = r#"
repo() { mkdir -p "$1" && g -C "$1" init -q --object-format=$FORMAT; }
repo inner; echo i > inner/f; g -C inner add -A; g -C inner commit -q -m inner
repo sub; echo s > sub/f; g -C sub add -A; g -C sub commit -q -m sub
g -C sub submodule add -q ../inner inner; g -C sub commit -q -m 'add inner'
repo super; echo t > super/top; g -C super add -A; g -C super commit -q -m top
g -C super submodule add -q ../sub sub; g -C super submodule update -q --init --recursive
g -C super commit -q -m 'add sub'
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

	fn gta_status(&self, args: &[&str]) -> Output {
		let mut command = Command::new(assert_cmd::cargo::cargo_bin("gta"));
		self.isolated(&mut command);
		command
			.arg("-C")
			.arg(self.superproject())
			.arg("status")
			.args(args)
			.output()
			.unwrap()
	}

	/// gta's status must print git's, or both must fail.
	fn check(&self, label: &str, args: &[&str]) {
		let label = format!("{label} ({})", self.format);
		let git = self.git_status(args);
		let gta = self.gta_status(args);
		match git.status.code() {
			Some(0) => {
				assert!(gta.status.success(), "{label}: gta failed: {gta:?}");
				assert_eq!(
					String::from_utf8_lossy(&gta.stdout),
					String::from_utf8_lossy(&git.stdout),
					"{label}"
				);
				assert_eq!(warnings(&gta), warnings(&git), "{label}: warnings");
			}
			_ => assert!(
				!gta.status.success(),
				"{label}: git failed ({git:?}), gta didn't ({gta:?})"
			),
		}
	}
}

/// The distinct `warning:` lines a command printed.
fn warnings(out: &Output) -> Vec<String> {
	let mut warnings: Vec<String> = Vec::new();
	for line in String::from_utf8_lossy(&out.stderr).lines() {
		if line.starts_with("warning: ") && !warnings.iter().any(|known| known == line) {
			warnings.push(line.to_owned());
		}
	}
	warnings
}

#[test]
fn reports_submodule_content_as_git_does() {
	for format in ["sha1", "sha256"] {
		for (label, change) in [
			("clean", "true"),
			("modified content", "echo y >> sub/f"),
			("untracked content", "echo y > sub/new"),
			("nested modified content", "echo y >> sub/inner/f"),
			(
				"new commits",
				"echo y >> sub/f; g -C sub commit -q -am moved",
			),
		] {
			Fixture::new(format, change).check(label, &[]);
		}
	}
}

#[test]
fn takes_ignore_submodules_as_git_does() {
	let dirty = "echo y >> sub/f; echo y > sub/new";
	let staged = "echo y >> sub/f; g -C sub commit -q -am moved; g add sub";
	for format in ["sha1", "sha256"] {
		for when in ["none", "untracked", "dirty", "all"] {
			let flag = format!("--ignore-submodules={when}");
			Fixture::new(format, dirty).check(&format!("{flag}, dirty"), &[&flag]);
			Fixture::new(format, staged).check(&format!("{flag}, staged"), &[&flag]);
		}
		// No value means `all`.
		Fixture::new(format, staged).check("bare flag, staged", &["--ignore-submodules"]);
		// The flag outranks the submodule's own setting.
		Fixture::new(
			format,
			&format!("g config submodule.sub.ignore all; {dirty}"),
		)
		.check("flag none over config all", &["--ignore-submodules=none"]);
		// A value git doesn't know.
		let fixture = Fixture::new(format, dirty);
		fixture.check("an unknown value", &["--ignore-submodules=bogus"]);
		let gta = fixture.gta_status(&["--ignore-submodules=bogus"]);
		assert!(
			String::from_utf8_lossy(&gta.stderr).contains("bad --ignore-submodules argument: bogus"),
			"{format}: {gta:?}"
		);
	}
}

#[test]
fn prints_git_s_warnings_and_refuses_what_git_refuses() {
	let declared = |extra: &str| {
		format!(
			r#"echo y >> sub/f; printf '[submodule "sub"]\n\tpath = sub\n\turl = ../sub\n{extra}' > .gitmodules"#
		)
	};
	for format in ["sha1", "sha256"] {
		Fixture::new(format, &declared(r"\tignore = bogus\n")).check("an invalid ignore", &[]);
		Fixture::new(
			format,
			&declared(r#"[submodule "../evil"]\n\tpath = evil\n"#),
		)
		.check("a suspicious name", &[]);
		Fixture::new(format, &declared(r"\tshallow = invalid\n")).check("a bad shallow", &[]);
		Fixture::new(format, "g config diff.ignoreSubmodules bogus")
			.check("diff.ignoreSubmodules=bogus", &[]);
		Fixture::new(format, "g config diff.ignoreSubmodules bogus").check(
			"diff.ignoreSubmodules=bogus, flag none",
			&["--ignore-submodules=none"],
		);
		Fixture::new(format, "g config submodule.sub.ignore bogus")
			.check("submodule.sub.ignore=bogus", &[]);
		// The submodule's own setting: its status refuses, so the superproject's does, unless the
		// setting in effect doesn't look inside it.
		for when in ["", "none", "untracked", "dirty", "all"] {
			let flag = format!("--ignore-submodules={when}");
			let args: &[&str] = if when.is_empty() { &[] } else { &[&flag] };
			for set in [
				"g -C sub config diff.ignoreSubmodules bogus",
				r"printf '[diff]\n\tignoreSubmodules\n' >> .git/modules/sub/config",
			] {
				Fixture::new(format, &format!("echo y >> sub/f; {set}"))
					.check(&format!("{set}, {args:?}"), args);
			}
		}
	}
}
