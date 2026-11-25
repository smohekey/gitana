#![cfg(unix)]
//! `gta submodule deinit` of a submodule with submodules inside it, against git 2.55's on an
//! identical copy, in both object formats: what's refused, what's cleared and kept, and the nested
//! `.git` directories git absorbs first.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A superproject whose submodule `sub` holds `inner`, which holds `deep`, all initialized, as
/// `git submodule update --init --recursive` makes them. `$NAME` is the name `sub`'s `.gitmodules`
/// gives `inner`.
const FIXTURE: &str = r#"
repo() { mkdir -p "$1" && g -C "$1" init -q --object-format=$FORMAT; }
repo deep; echo d > deep/f; g -C deep add -A; g -C deep commit -q -m deep
repo inner; echo i > inner/f; g -C inner add -A; g -C inner commit -q -m inner
g -C inner submodule add -q ../deep deep; g -C inner commit -q -m 'add deep'
repo sub; echo s > sub/f; echo '*.log' > sub/.gitignore; g -C sub add -A; g -C sub commit -q -m sub
g -C sub submodule add -q --name "${NAME:-inner}" ../inner inner; g -C sub commit -q -m 'add inner'
repo super; echo t > super/top; g -C super add -A; g -C super commit -q -m top
g -C super submodule add -q ../sub sub; g -C super submodule update -q --init --recursive
g -C super commit -q -m 'add sub'
"#;

/// Replaces `sub/inner` (and `deep` inside it) with plain clones, whose `.git` are directories, as
/// if they'd never been initialized through `sub`: their module stores go too.
const CLONES: &str = "rm -rf sub/inner .git/modules/sub/modules; g clone -q ../inner sub/inner; \
	rm -rf sub/inner/deep; g clone -q ../deep sub/inner/deep";

const PRELUDE: &str = r#"
set -e
g() { git -c user.name=T -c user.email=t@example.com -c commit.gpgsign=false -c init.defaultBranch=main -c protocol.file.allow=always "$@"; }
"#;

struct Fixture {
	base: tempfile::TempDir,
	format: &'static str,
}

impl Fixture {
	fn new(format: &'static str, name: &str, change: &str) -> Self {
		let fixture = Self {
			base: tempfile::tempdir().unwrap(),
			format,
		};
		std::fs::create_dir_all(fixture.home()).unwrap();
		// gta's deinit reads its config inputs as regular files, so the global one is an empty file.
		std::fs::write(fixture.home().join("gitconfig"), "").unwrap();
		fixture.sh(fixture.base.path(), &format!("NAME={name}; {FIXTURE}"));
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
			.env("GIT_CONFIG_GLOBAL", self.home().join("gitconfig"))
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

	fn deinit(&self, program: &str) -> Output {
		let mut command = if program == "gta" {
			Command::new(assert_cmd::cargo::cargo_bin("gta"))
		} else {
			Command::new("git")
		};
		self.isolated(&mut command);
		command
			.current_dir(self.superproject())
			.args(["submodule", "deinit", "sub"])
			.output()
			.unwrap()
	}

	/// What a deinit leaves: the mount's entries, what each nested `.git` is, and which module stores
	/// there are.
	fn state(&self) -> String {
		let root = self.superproject();
		let mut entries: Vec<String> = std::fs::read_dir(root.join("sub"))
			.map(|entries| {
				entries
					.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
					.collect()
			})
			.unwrap_or_default();
		entries.sort();
		let kind = |path: &str| {
			let marker = root.join(path).join(".git");
			match std::fs::symlink_metadata(&marker) {
				Ok(meta) if meta.is_dir() => "directory",
				Ok(_) => "gitfile",
				Err(_) => "none",
			}
		};
		let stores: Vec<&str> = [
			".git/modules/sub",
			".git/modules/sub/modules/inner",
			".git/modules/sub/modules/other",
			".git/modules/sub/modules/inner/modules/deep",
			".git/modules/sub/modules/other/modules/deep",
		]
		.into_iter()
		.filter(|store| root.join(store).join("HEAD").exists())
		.collect();
		format!(
			"mount={entries:?} inner={} deep={} stores={stores:?}",
			kind("sub/inner"),
			kind("sub/inner/deep")
		)
	}

	/// git's `Migrating git directory` lines, with the fixture's own folder taken out.
	fn migrations(&self, out: &Output) -> Vec<String> {
		let base = self.base.path().canonicalize().unwrap();
		String::from_utf8_lossy(&out.stderr)
			.replace(&base.display().to_string(), "BASE")
			.lines()
			.map(str::to_owned)
			.collect::<Vec<_>>()
			.windows(3)
			.filter(|lines| lines[0].starts_with("Migrating git directory"))
			.map(|lines| lines.join("\n"))
			.collect()
	}
}

/// Runs the deinit with git on one copy and gta on another, and asserts the two agree: both clear
/// or both refuse, and leave the same state; and, when both clear, report the same directories
/// absorbed. (git also prints its `Migrating` lines before a refusal; gta reports them only with a
/// finished deinit.)
fn compare(format: &'static str, label: &str, name: &str, change: &str) -> (Output, Output) {
	let label = format!("{label} ({format})");
	let git = Fixture::new(format, name, change);
	let git_out = git.deinit("git");
	let gta = Fixture::new(format, name, change);
	let gta_out = gta.deinit("gta");
	assert_eq!(
		gta_out.status.success(),
		git_out.status.success(),
		"{label}: git {git_out:?}\ngta {gta_out:?}"
	);
	assert_eq!(gta.state(), git.state(), "{label}: what's left");
	if git_out.status.success() {
		assert_eq!(
			gta.migrations(&gta_out),
			git.migrations(&git_out),
			"{label}: absorbed"
		);
	}
	(git_out, gta_out)
}

#[test]
fn clears_or_refuses_nested_submodules_as_git_does() {
	for format in ["sha1", "sha256"] {
		for (label, change) in [
			("clean, two levels nested", "true"),
			("ignored files only", "echo y > sub/a.log"),
			("modified", "echo y >> sub/f"),
			("inner modified", "echo y >> sub/inner/f"),
			("inner untracked", "echo y > sub/inner/new"),
			(
				"inner new commits",
				"echo y >> sub/inner/f; g -C sub/inner commit -q -am moved",
			),
			("deep untracked", "echo y > sub/inner/deep/new"),
			(
				"inner's own setting doesn't matter",
				"echo y >> sub/inner/f; g -C sub config -f .gitmodules submodule.inner.ignore all",
			),
			("inner uninitialized", "g -C sub submodule deinit -q inner"),
		] {
			compare(format, label, "inner", change);
		}
	}
}

#[test]
fn absorbs_nested_git_directories_as_git_does() {
	for format in ["sha1", "sha256"] {
		let (_, gta) = compare(format, "clean clones", "inner", CLONES);
		assert!(gta.status.success(), "{format}: {gta:?}");
		compare(format, "clean clones, named 'other'", "other", CLONES);
		// Absorbed, then refused: what was moved stays moved.
		let (_, gta) = compare(
			format,
			"a clone with a commit of its own",
			"inner",
			&format!("{CLONES}; echo y >> sub/inner/f; g -C sub/inner commit -q -am own"),
		);
		assert!(!gta.status.success(), "{format}: {gta:?}");
	}
}

#[test]
fn refuses_what_git_cannot_absorb() {
	for format in ["sha1", "sha256"] {
		let (_, gta) = compare(
			format,
			"no declaration",
			"inner",
			&format!(
				"g -C sub config -f .gitmodules --remove-section submodule.inner; \
				 g -C sub commit -q -am undeclared; {CLONES}"
			),
		);
		assert!(
			String::from_utf8_lossy(&gta.stderr).contains("could not lookup name for submodule 'inner'"),
			"{format}: {gta:?}"
		);
		let (_, gta) = compare(
			format,
			"a destination already there",
			"inner",
			&format!(
				"{CLONES}; mkdir -p .git/modules/sub/modules/inner; \
				 echo x > .git/modules/sub/modules/inner/stray"
			),
		);
		assert!(
			String::from_utf8_lossy(&gta.stderr).contains("Directory not empty"),
			"{format}: {gta:?}"
		);
	}
}
