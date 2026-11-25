#![cfg(unix)]
//! A bad or valueless `diff.ignoreSubmodules`, command by command, against git 2.55 on identical
//! copies, in both object formats. Where git refuses (its commands that read diff settings, and
//! every status it computes), gta refuses with git's message. Where git proceeds, the setting changes
//! nothing for gta: it does what it does without it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A superproject `super` with a submodule `sub`, a remote `origin` it tracks, branches `topic`
/// (diverged) and `ff` (ahead), a stash entry, a tag, a linked worktree `wt`, and a bare copy
/// `bare.git`.
const FIXTURE: &str = r#"
g init -q --object-format=$FORMAT sub-origin
(cd sub-origin && echo s > f && g add -A && g commit -q -m s)
g init -q --bare --object-format=$FORMAT origin.git
g init -q --object-format=$FORMAT super
cd super
mkdir dir; echo a > a; echo d > dir/f; echo r > rmme; g add -A; g commit -q -m a
g submodule add -q ../sub-origin sub; g commit -q -m sub
g remote add origin ../origin.git; g push -q -u origin main
g branch topic; g branch ff
echo b > b; g add -A; g commit -q -m b
g checkout -q ff; g merge -q --ff-only main; echo f2 > f2; g add -A; g commit -q -m f2
g checkout -q topic; echo c > c; g add -A; g commit -q -m c; g checkout -q main
echo st >> a; g stash -q
g tag v0
g worktree add -q --detach ../wt topic
cd ..; g clone -q --bare super bare.git
"#;

/// Commands git refuses with a bad setting, as `(label, git's, gta's)`. `G` runs the tool, `-c`
/// settings included.
const REFUSED: &[(&str, &str, &str)] = &[
	("status", "G status --porcelain", "G status"),
	(
		"commit with a change",
		"echo x >> a; G add a; G commit -m x",
		"echo x >> a; G add a; G commit -m x",
	),
	(
		"commit with nothing to commit",
		"G commit -m x",
		"G commit -m x",
	),
	("merge, fast-forward", "G merge ff", "G merge ff"),
	("merge, up to date", "G merge main~1", "G merge main~1"),
	(
		"merge, a true merge",
		"G merge -m m topic",
		"G merge -m m topic",
	),
	("log", "G log", "G log"),
	(
		"log in a bare repository",
		"G -C ../bare.git log",
		"G -C ../bare.git log",
	),
	("show", "G show HEAD", "G show HEAD"),
	(
		"show in a bare repository",
		"G -C ../bare.git show HEAD",
		"G -C ../bare.git show HEAD",
	),
	(
		"show of a revision that doesn't resolve",
		"G show nothing-here",
		"G show nothing-here",
	),
	("switch", "G switch topic", "G switch topic"),
	("switch -c", "G switch -c new", "G switch -c new"),
	(
		"checkout of a branch",
		"G checkout topic",
		"G checkout topic",
	),
	(
		"checkout of a path",
		"echo x >> a; G checkout -- a",
		"echo x >> a; G checkout -- a",
	),
	(
		"restore",
		"echo x >> a; G restore a",
		"echo x >> a; G restore a",
	),
	(
		"restore --staged",
		"echo x >> a; G add a; G restore --staged a",
		"echo x >> a; G add a; G restore --staged a",
	),
	("diff", "echo x >> a; G diff", "echo x >> a; G diff"),
	(
		"diff --cached",
		"echo x >> a; G add a; G diff --cached",
		"echo x >> a; G add a; G diff --cached",
	),
	("pull", "G pull", "G pull"),
	(
		"worktree remove",
		"G worktree remove ../wt",
		"G worktree remove ../wt",
	),
	("stash list", "G stash list", "G stash list"),
	(
		"stash show",
		"G stash show --name-only",
		"G stash show --name-only",
	),
];

/// Commands git runs whatever the setting, as `(label, git's, gta's)`.
const PROCEEDS: &[(&str, &str, &str)] = &[
	("hash-object", "G hash-object a", "G hash-object a"),
	("cat-file", "G cat-file -p HEAD", "G cat-file -p HEAD"),
	("ls-tree", "G ls-tree HEAD", "G ls-tree HEAD"),
	("rev-parse", "G rev-parse HEAD", "G rev-parse HEAD"),
	("rev-list", "G rev-list HEAD", "G rev-list HEAD"),
	(
		"merge-base",
		"G merge-base main topic",
		"G merge-base main topic",
	),
	(
		"update-ref",
		"G update-ref refs/heads/x HEAD",
		"G update-ref refs/heads/x HEAD",
	),
	("symbolic-ref", "G symbolic-ref HEAD", "G symbolic-ref HEAD"),
	("ls-files", "G ls-files", "G ls-files"),
	("add", "echo x >> a; G add a", "echo x >> a; G add a"),
	("rm", "G rm rmme", "G rm rmme"),
	("mv", "G mv a a2", "G mv a a2"),
	("cherry-pick", "G cherry-pick topic", "G cherry-pick topic"),
	("revert", "G revert --no-edit HEAD", "G revert HEAD"),
	("rebase", "G rebase topic", "G rebase topic"),
	("repack", "G repack -q", "G repack"),
	("prune", "G prune", "G prune"),
	("gc", "G gc -q", "G gc"),
	("config --list", "G config --list", "G config --list"),
	("config, set", "G config x.y z", "G config x.y z"),
	("branch, list", "G branch", "G branch"),
	("branch, create", "G branch new", "G branch new"),
	("tag, list", "G tag", "G tag"),
	("tag, create", "G tag v1", "G tag v1"),
	(
		"reset --soft",
		"G reset --soft HEAD~1",
		"G reset --soft HEAD~1",
	),
	(
		"reset --hard",
		"G reset --hard HEAD~1",
		"G reset --hard HEAD~1",
	),
	(
		"reset of a path",
		"echo x >> a; G add a; G reset -- a",
		"echo x >> a; G add a; G reset -- a",
	),
	(
		"clone",
		"cd ..; G clone super clone2",
		"cd ..; G clone super clone2",
	),
	("fetch", "G fetch", "G fetch"),
	("push", "G push origin main", "G push origin main"),
	(
		"submodule status",
		"G submodule status",
		"G submodule status",
	),
	("submodule init", "G submodule init", "G submodule init"),
	(
		"submodule update",
		"G submodule update",
		"G submodule update",
	),
	// The submodule's status doesn't see the superproject's own config.
	(
		"submodule deinit",
		"G submodule deinit sub",
		"G submodule deinit sub",
	),
	("submodule sync", "G submodule sync", "G submodule sync"),
	("remote, list", "G remote -v", "G remote -v"),
	(
		"remote add",
		"G remote add other ../x",
		"G remote add other ../x",
	),
	(
		"remote remove",
		"G remote remove origin",
		"G remote remove origin",
	),
	(
		"remote rename",
		"G remote rename origin up",
		"G remote rename origin up",
	),
	("worktree list", "G worktree list", "G worktree list"),
	(
		"worktree remove --force",
		"G worktree remove --force ../wt",
		"G worktree remove --force ../wt",
	),
	(
		"worktree lock",
		"G worktree lock ../wt",
		"G worktree lock ../wt",
	),
	("worktree prune", "G worktree prune", "G worktree prune"),
	(
		"worktree move",
		"G worktree move ../wt ../wt3",
		"G worktree move ../wt ../wt3",
	),
	("worktree repair", "G worktree repair", "G worktree repair"),
	(
		"sparse-checkout set",
		"G sparse-checkout set dir",
		"G sparse-checkout set dir",
	),
	(
		"sparse-checkout list",
		"G sparse-checkout set dir; G sparse-checkout list",
		"G sparse-checkout set dir; G sparse-checkout list",
	),
	(
		"sparse-checkout disable",
		"G sparse-checkout set dir; G sparse-checkout disable",
		"G sparse-checkout set dir; G sparse-checkout disable",
	),
	("stash push", "echo x >> a; G stash", "echo x >> a; G stash"),
	("stash apply", "G stash apply", "G stash apply"),
	("stash pop", "G stash pop", "G stash pop"),
	("stash drop", "G stash drop", "G stash drop"),
];

/// How the bad setting is given.
#[derive(Clone, Copy)]
enum Setting {
	/// Not at all, for what a command does without one.
	Unset,
	/// `diff.ignoreSubmodules = bogus` in the superproject's (and the bare copy's) config.
	Bad,
	/// `diff.ignoreSubmodules` with no value, there.
	Valueless,
	/// `-c diff.ignoreSubmodules=bogus` on the command line.
	CommandLine,
	/// `diff.ignoreSubmodules = bogus` in the global config.
	Global,
}

/// The fixture for one object format: built once, and put back before each run.
struct Fixture {
	base: tempfile::TempDir,
	format: &'static str,
}

impl Fixture {
	fn new(format: &'static str) -> Self {
		let fixture = Self {
			base: tempfile::tempdir().unwrap(),
			format,
		};
		std::fs::create_dir_all(fixture.home()).unwrap();
		std::fs::create_dir_all(fixture.case()).unwrap();
		fixture.write_global("");
		fixture.sh(&fixture.case(), "git", Setting::Unset, FIXTURE, true);
		// The linked worktree's paths are absolute, so each run gets the fixture back in place.
		fixture.tar(&["-cf", "../template.tar", "."]);
		fixture
	}

	fn home(&self) -> PathBuf {
		self.base.path().join("home")
	}

	fn case(&self) -> PathBuf {
		self.base.path().join("case")
	}

	fn tar(&self, args: &[&str]) {
		let status = Command::new("tar")
			.args(args)
			.current_dir(self.case())
			.status()
			.unwrap();
		assert!(status.success(), "tar {args:?}");
	}

	fn restore(&self) {
		std::fs::remove_dir_all(self.case()).unwrap();
		std::fs::create_dir_all(self.case()).unwrap();
		self.tar(&["-xf", "../template.tar"]);
	}

	/// The global config: who commits, and `extra`.
	fn write_global(&self, extra: &str) {
		std::fs::write(
			self.home().join("gitconfig"),
			format!(
				"[user]\n\tname = T\n\temail = t@example.com\n[commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n[protocol \"file\"]\n\tallow = always\n[maintenance]\n\tauto = false\n[gc]\n\tauto = 0\n{extra}"
			),
		)
		.unwrap();
	}

	/// Run `script` in `dir` with `G` running `tool` ("git" or the gta binary) under `setting`.
	fn sh(&self, dir: &Path, tool: &str, setting: Setting, script: &str, must: bool) -> Output {
		let extra = match setting {
			Setting::CommandLine => "-c diff.ignoreSubmodules=bogus",
			_ => "",
		};
		let out = Command::new("sh")
			.current_dir(dir)
			.env("FORMAT", self.format)
			.env("HOME", self.home())
			.env("XDG_CONFIG_HOME", self.home())
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("GIT_CONFIG_GLOBAL", self.home().join("gitconfig"))
			.env("GIT_ATTR_NOSYSTEM", "1")
			.env("GIT_EDITOR", "true")
			.env("GIT_PAGER", "cat")
			.env("GIT_TERMINAL_PROMPT", "0")
			.env("TOOL", tool)
			.env("EXTRA", extra)
			.arg("-c")
			.arg(format!(
				"g() {{ git \"$@\"; }}\nG() {{ \"$TOOL\" $EXTRA \"$@\"; }}\n{}{script}",
				if must { "set -e\n" } else { "" }
			))
			.output()
			.unwrap();
		if must {
			assert!(
				out.status.success(),
				"{script}\n{}",
				String::from_utf8_lossy(&out.stderr)
			);
		}
		out
	}

	/// `script` run by `tool` on a fresh copy, with the setting given as `setting` says.
	fn run(&self, tool: &str, setting: Setting, script: &str) -> Output {
		self.restore();
		let super_dir = self.case().join("super");
		self.write_global(match setting {
			Setting::Global => "[diff]\n\tignoreSubmodules = bogus\n",
			_ => "",
		});
		for config in [
			super_dir.join(".git/config"),
			self.case().join("bare.git/config"),
		] {
			let line = match setting {
				Setting::Bad => "[diff]\n\tignoreSubmodules = bogus\n",
				Setting::Valueless => "[diff]\n\tignoreSubmodules\n",
				_ => continue,
			};
			let mut text = std::fs::read_to_string(&config).unwrap();
			text.push_str(line);
			std::fs::write(&config, text).unwrap();
		}
		self.sh(&super_dir, tool, setting, script, false)
	}
}

fn gta() -> String {
	assert_cmd::cargo::cargo_bin("gta").display().to_string()
}

/// git's refusal in `out`, without its `fatal: ` or `error: ` prefix, if it refused.
fn refusal(out: &Output) -> Option<String> {
	String::from_utf8_lossy(&out.stderr)
		.lines()
		.find_map(|line| {
			[
				"bad --ignore-submodules argument",
				"missing value for 'diff.ignoresubmodules'",
			]
			.iter()
			.find_map(|text| line.find(text).map(|at| line[at..].to_owned()))
		})
}

fn refuses_where_git_does(settings: &[Setting]) {
	for format in ["sha1", "sha256"] {
		let fixture = Fixture::new(format);
		for &(label, git_script, gta_script) in REFUSED {
			for &setting in settings {
				let git = fixture.run("git", setting, git_script);
				let gta_out = fixture.run(&gta(), setting, gta_script);
				let expected =
					refusal(&git).unwrap_or_else(|| panic!("{label} ({format}): git didn't refuse: {git:?}"));
				assert_eq!(
					refusal(&gta_out).as_deref(),
					Some(expected.as_str()),
					"{label} ({format}): gta {gta_out:?}"
				);
				assert!(!gta_out.status.success(), "{label} ({format})");
			}
		}
	}
}

#[test]
fn refuses_a_bad_setting_where_git_does() {
	refuses_where_git_does(&[Setting::Bad, Setting::Valueless]);
}

#[test]
fn refuses_a_setting_from_the_command_line_or_the_global_config() {
	refuses_where_git_does(&[Setting::CommandLine, Setting::Global]);
}

#[test]
fn proceeds_with_a_bad_setting_where_git_does() {
	for format in ["sha1", "sha256"] {
		let fixture = Fixture::new(format);
		for &(label, git_script, gta_script) in PROCEEDS {
			let git_without = fixture.run("git", Setting::Unset, git_script);
			let without = fixture.run(&gta(), Setting::Unset, gta_script);
			for setting in [Setting::Bad, Setting::Valueless] {
				// git does what it does without the setting. (Its `stash apply` and `pop` still print
				// the refusal of the status they show afterwards, having applied the stash.)
				let git = fixture.run("git", setting, git_script);
				assert_eq!(
					git.status.success(),
					git_without.status.success(),
					"{label} ({format}): the setting changed what git did: {git:?}"
				);
				let gta_out = fixture.run(&gta(), setting, gta_script);
				assert_eq!(
					refusal(&gta_out),
					None,
					"{label} ({format}): gta refused: {gta_out:?}"
				);
				assert_eq!(
					gta_out.status.success(),
					without.status.success(),
					"{label} ({format}): the setting changed what gta did: {gta_out:?}\nwithout it: {without:?}"
				);
			}
		}
	}
}
