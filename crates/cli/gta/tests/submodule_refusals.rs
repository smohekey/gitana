#![cfg(unix)]
//! `gta submodule deinit`, `sync` and `update` say each part of a refusal once: the wrapper's own
//! context, then the reason, never the reason again.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A repository `r` with the submodule `sub`, cloned from `sub-origin`, committed.
const FIXTURE: &str = r#"
g init -q sub-origin
(cd sub-origin && echo s > f && g add -A && g commit -q -m s)
g init -q r
cd r
echo t > t; g add -A; g commit -q -m t
g submodule add -q ../sub-origin sub; g commit -q -m sub
"#;

const PRELUDE: &str = r#"
set -e
g() { git -c user.name=T -c user.email=t@example.com -c commit.gpgsign=false -c init.defaultBranch=main -c protocol.file.allow=always "$@"; }
"#;

struct Fixture {
	base: tempfile::TempDir,
}

impl Fixture {
	fn new(change: &str) -> Self {
		let fixture = Self {
			base: tempfile::tempdir().unwrap(),
		};
		std::fs::create_dir_all(fixture.home()).unwrap();
		fixture.sh(fixture.base.path(), FIXTURE);
		fixture.sh(&fixture.repository(), change);
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

	fn gta(&self, args: &[&str]) -> Output {
		let mut command = Command::new(assert_cmd::cargo::cargo_bin("gta"));
		self.isolated(&mut command);
		command
			.current_dir(self.repository())
			.args(["-c", "protocol.file.allow=always"])
			.args(args)
			.output()
			.unwrap()
	}
}

/// `out` failed, and its `gta:` line says each of `parts` exactly once, in that order.
fn says_once(label: &str, out: &Output, parts: &[&str]) {
	assert!(!out.status.success(), "{label}: {out:?}");
	let stderr = String::from_utf8_lossy(&out.stderr);
	let line = stderr
		.lines()
		.find(|line| line.starts_with("gta: "))
		.unwrap_or_else(|| panic!("{label}: no refusal in {stderr}"));
	let mut rest = line;
	for part in parts {
		assert_eq!(line.matches(part).count(), 1, "{label}: {part:?} in {line}");
		let at = rest
			.find(part)
			.unwrap_or_else(|| panic!("{label}: {part:?} out of order in {line}"));
		rest = &rest[at + part.len()..];
	}
}

#[test]
fn deinit_says_its_refusal_once() {
	let fixture = Fixture::new("echo y >> sub/f");
	says_once(
		"deinit",
		&fixture.gta(&["submodule", "deinit", "sub"]),
		&["submodule path 'sub' contains local modifications"],
	);
}

#[test]
fn sync_says_its_refusal_once() {
	let fixture =
		Fixture::new("g config -f .gitmodules submodule.sub.url -bad; g commit -q -am badurl");
	says_once(
		"sync",
		&fixture.gta(&["submodule", "sync"]),
		&[
			"synchronizing submodule 'sub'",
			"no URL found in .gitmodules for submodule path 'sub'",
		],
	);
}

#[test]
fn update_says_its_refusal_once() {
	// The recorded commit was never pushed to the origin, so the update can't fetch it.
	let fixture = Fixture::new(
		"(cd sub && echo y >> f && g commit -q -am local); g add sub; g commit -q -m bump; \
		 g submodule deinit -q -f sub; rm -rf .git/modules/sub",
	);
	says_once(
		"update",
		&fixture.gta(&["submodule", "update", "--init", "sub"]),
		&["updating submodule 'sub'", "repository transfer failed"],
	);
}
