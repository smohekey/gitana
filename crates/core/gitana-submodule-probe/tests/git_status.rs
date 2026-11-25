#![cfg(unix)]
//! gitana's status, with the native probe looking inside submodules, against `git status
//! --porcelain` on the same repositories, in both object formats. Each case is a shell snippet run in
//! the superproject (with `g` for an isolated git) that leaves it in some state; the two statuses
//! must print the same, or both fail, and gitana's warnings must be git's.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, OnceLock};

use cap_std::{ambient_authority, fs::Dir};
use gitana_config_native::Environment;
use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};
use gitana_object::{HashAlgorithm, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_repository::Repository;
use gitana_submodule_probe::NativeSubmoduleProbe;
use gitana_worktree::{IgnoreSubmodules, OutsideAttributes, WorkTree};

/// A superproject with the submodule `sub` (a tracked `f`, a `.gitignore` ignoring `*.log`, and
/// a nested submodule `inner` with a tracked `f`), all committed, as `git submodule add` makes it.
const FIXTURE: &str = r#"
repo() { mkdir -p "$1" && g -C "$1" init -q --object-format=$FORMAT; }
repo inner; echo i > inner/f; g -C inner add -A; g -C inner commit -q -m inner
repo sub; echo s > sub/f; echo '*.log' > sub/.gitignore; g -C sub add -A; g -C sub commit -q -m sub
g -C sub submodule add -q ../inner inner; g -C sub commit -q -m 'add inner'
repo super; echo t > super/top; g -C super add -A; g -C super commit -q -m top
g -C super submodule add -q ../sub sub; g -C super submodule update -q --init --recursive
g -C super commit -q -m 'add sub'
"#;

/// The shell prelude every snippet runs under: an isolated git, as `g`.
const PRELUDE: &str = r#"
set -e
g() { git -c user.name=T -c user.email=t@example.com -c commit.gpgsign=false -c init.defaultBranch=main -c protocol.file.allow=always "$@"; }
"#;

struct Fixture {
	base: tempfile::TempDir,
	format: &'static str,
}

impl Fixture {
	/// A fresh copy of the format's template, which is built once (git keeps a submodule's paths
	/// relative, so a copy works where the template did).
	fn new(format: &'static str) -> Self {
		static SHA1: OnceLock<Fixture> = OnceLock::new();
		static SHA256: OnceLock<Fixture> = OnceLock::new();
		let template = match format {
			"sha1" => &SHA1,
			_ => &SHA256,
		}
		.get_or_init(|| {
			let template = Self::empty(format);
			template.sh(template.base.path(), FIXTURE);
			template
		});
		let fixture = Self::empty(format);
		let copied = Command::new("cp")
			.arg("-Rp")
			.arg(template.base.path().join("."))
			.arg(fixture.base.path())
			.status()
			.unwrap();
		assert!(copied.success());
		fixture
	}

	fn empty(format: &'static str) -> Self {
		let fixture = Self {
			base: tempfile::tempdir().unwrap(),
			format,
		};
		std::fs::create_dir_all(fixture.home().join("xdg")).unwrap();
		fixture
	}

	fn home(&self) -> PathBuf {
		self.base.path().join("home")
	}

	fn superproject(&self) -> PathBuf {
		self.base.path().join("super")
	}

	fn command(&self, program: &str, dir: &Path) -> Command {
		let mut command = Command::new(program);
		command
			.current_dir(dir)
			.env("FORMAT", self.format)
			.env("BASE", self.base.path())
			.env("HOME", self.home())
			.env("XDG_CONFIG_HOME", self.home().join("xdg"))
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_ATTR_NOSYSTEM", "1")
			.env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
			.env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z");
		command
	}

	fn sh(&self, dir: &Path, script: &str) {
		let out = self
			.command("sh", dir)
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

	fn git_status(&self, dir: &Path, args: &[&str]) -> Output {
		self
			.command("git", dir)
			.args(["status", "--porcelain"])
			.args(args)
			.output()
			.unwrap()
	}

	fn environment(&self) -> Environment {
		Environment {
			home: Some(self.home()),
			xdg_config_home: Some(self.home().join("xdg")),
			config_global: Some(PathBuf::from("/dev/null")),
			config_nosystem: true,
			attr_nosystem: true,
		}
	}

	/// gitana's status of the repository at `dir`, with the probe, and the probe's warnings.
	async fn gitana_status(
		&self,
		dir: &Path,
		flag: Option<IgnoreSubmodules>,
	) -> Result<(String, Vec<String>), String> {
		let run = async {
			match self.format {
				"sha1" => status::<Sha1>(dir, flag).await,
				_ => status::<Sha256>(dir, flag).await,
			}
		};
		gitana_config_native::with_environment(self.environment(), run).await
	}

	/// Leaves the superproject as `setup` does, then asserts gitana's status matches git's.
	async fn check(&self, label: &str, setup: &str, args: &[&str]) {
		self.check_at(label, setup, "super", args).await;
	}

	async fn check_at(&self, label: &str, setup: &str, dir: &str, args: &[&str]) {
		let label = format!("{label} ({})", self.format);
		self.sh(&self.superproject(), setup);
		let dir = self.base.path().join(dir);
		let git = self.git_status(&dir, args);
		let ours = self.gitana_status(&dir, flag(args)).await;
		let stderr = String::from_utf8_lossy(&git.stderr);
		match (git.status.code(), ours) {
			(Some(0), Ok((porcelain, warnings))) => {
				assert_eq!(porcelain, String::from_utf8_lossy(&git.stdout), "{label}");
				let mut expected: Vec<String> = Vec::new();
				for warning in stderr
					.lines()
					.filter_map(|line| line.strip_prefix("warning: "))
				{
					if !expected.iter().any(|known| known == warning) {
						expected.push(warning.to_owned());
					}
				}
				assert_eq!(warnings, expected, "{label}: warnings");
			}
			(Some(128), Err(error)) => {
				assert!(error.contains("submodule 'sub'"), "{label}: {error}");
			}
			(code, ours) => panic!("{label}: git exited {code:?} ({stderr}), gitana gave {ours:?}"),
		}
	}
}

fn flag(args: &[&str]) -> Option<IgnoreSubmodules> {
	args
		.iter()
		.find_map(|arg| match arg.strip_prefix("--ignore-submodules") {
			Some("") => Some(IgnoreSubmodules::All),
			Some(value) => IgnoreSubmodules::parse(value.strip_prefix('=').unwrap()),
			None => None,
		})
}

fn open(path: &Path) -> Dir {
	Dir::open_ambient_dir(path, ambient_authority()).unwrap()
}

/// The repository at `dir`, opened as a frontend opens it, and its status with the probe.
async fn status<H: HashAlgorithm>(
	dir: &Path,
	flag: Option<IgnoreSubmodules>,
) -> Result<(String, Vec<String>), String> {
	let layout = gitana_repository_layout::discover(dir).await.unwrap();
	let root = layout.worktree_root.clone().unwrap();
	let config = gitana_config_native::for_worktree_at(
		open(&layout.common_dir),
		open(&layout.git_dir),
		&layout.common_dir,
		&layout.git_dir,
	)
	.await
	.unwrap();
	let excludes = gitana_config_native::excludes_file_at(&config, open(&root), &root)
		.await
		.unwrap();
	let attributes = gitana_config_native::attributes_files_at(&config, open(&root), &root)
		.await
		.unwrap();
	let probe = Arc::new(NativeSubmoduleProbe::<H>::new(
		layout.clone(),
		config.clone(),
	));
	let mut repository = Repository::new(ObjectStore::new(WorktreeFileStore::new(
		open(&layout.common_dir),
		open(&layout.git_dir),
	)));
	repository.set_effective_config(config);
	let mut tree = WorkTree::<_, _, H>::new_located(
		repository,
		CapWorkDir::from_dir(open(&root)),
		layout.git_dir.clone(),
		root,
	)
	.with_outside_attributes(OutsideAttributes {
		system: attributes.system,
		global: attributes.global,
	})
	.with_submodule_probe(probe.clone());
	if let Some(flag) = flag {
		tree = tree.with_ignore_submodules(flag);
	}
	let status = tree
		.status(excludes.as_deref())
		.await
		.map_err(|error| error.to_string())?;
	Ok((status.porcelain_v1(), probe.warnings()))
}

/// Runs `cases` (label, setup, status arguments), each on a fresh fixture, in both formats.
async fn each(cases: &[(&str, &str, &[&str])]) {
	for format in ["sha1", "sha256"] {
		for (label, setup, args) in cases {
			Fixture::new(format).check(label, setup, args).await;
		}
	}
}

const MOVED: &str = "echo x >> sub/f; g -C sub commit -q -am moved";

#[tokio::test]
async fn reports_each_state_as_git_does() {
	each(&[
		("clean", "true", &[]),
		("new commits", MOVED, &[]),
		("modified content", "echo x >> sub/f", &[]),
		("untracked content", "echo x > sub/new", &[]),
		(
			"modified and untracked",
			"echo x >> sub/f; echo x > sub/new",
			&[],
		),
		("ignored files only", "echo x > sub/a.log", &[]),
		("nested: inner modified", "echo x >> sub/inner/f", &[]),
		("nested: inner untracked", "echo x > sub/inner/new", &[]),
		(
			"nested: inner new commits",
			"echo x >> sub/inner/f; g -C sub/inner commit -q -am moved",
			&[],
		),
		(
			"staged gitlink and modified",
			&format!("{MOVED}; g add sub; echo y >> sub/f"),
			&[],
		),
		("staged gitlink, clean", &format!("{MOVED}; g add sub"), &[]),
		("mount deleted", "rm -rf sub", &[]),
		("mount replaced by a file", "rm -rf sub; echo f > sub", &[]),
		(
			"unborn HEAD in the submodule",
			"g -C sub symbolic-ref HEAD refs/heads/nowhere",
			&[],
		),
		(
			"uninitialized submodule",
			"g submodule deinit -q -f sub",
			&[],
		),
	])
	.await;
}

#[tokio::test]
async fn honours_each_ignore_setting_as_git_does() {
	let mut cases: Vec<(String, String, Vec<String>)> = Vec::new();
	for (place, set) in [
		(
			".gitmodules",
			"g config -f .gitmodules submodule.sub.ignore",
		),
		("config", "g config submodule.sub.ignore"),
		("diff", "g config diff.ignoreSubmodules"),
		("flag", ""),
	] {
		for value in ["none", "untracked", "dirty", "all"] {
			for (state, change) in [
				("modified+untracked", "echo x >> sub/f; echo x > sub/new"),
				("new commits", MOVED),
				("staged", &*format!("{MOVED}; g add sub")),
				("deleted", "rm -rf sub"),
			] {
				let (setup, args) = if place == "flag" {
					(
						change.to_owned(),
						vec![format!("--ignore-submodules={value}")],
					)
				} else {
					(format!("{change}; {set} {value}"), vec![])
				};
				cases.push((format!("{place}={value}, {state}"), setup, args));
			}
		}
	}
	cases.push((
		"bare flag".to_owned(),
		"echo x >> sub/f".to_owned(),
		vec!["--ignore-submodules".to_owned()],
	));
	for (label, setup) in [
		(
			".gitmodules=all, config=none",
			"g config -f .gitmodules submodule.sub.ignore all; g config submodule.sub.ignore none",
		),
		(
			".gitmodules=none, config=all",
			"g config -f .gitmodules submodule.sub.ignore none; g config submodule.sub.ignore all",
		),
		(
			".gitmodules=none, diff=all",
			"g config -f .gitmodules submodule.sub.ignore none; g config diff.ignoreSubmodules all",
		),
		(
			"config=none, diff=all",
			"g config submodule.sub.ignore none; g config diff.ignoreSubmodules all",
		),
		(
			"inner's own .gitmodules ignores it",
			"echo x >> sub/inner/f; g -C sub config -f .gitmodules submodule.inner.ignore all",
		),
	] {
		cases.push((
			label.to_owned(),
			format!("echo x >> sub/f; echo x > sub/new; echo x >> sub/inner/f; {setup}"),
			vec![],
		));
	}
	cases.push((
		"the flag doesn't reach the inner submodule".to_owned(),
		"echo x >> sub/inner/f; g -C sub config -f .gitmodules submodule.inner.ignore all".to_owned(),
		vec!["--ignore-submodules=none".to_owned()],
	));
	for format in ["sha1", "sha256"] {
		for (label, setup, args) in &cases {
			let args: Vec<&str> = args.iter().map(String::as_str).collect();
			Fixture::new(format).check(label, setup, &args).await;
		}
	}
}

#[tokio::test]
async fn opens_each_layout_as_git_does() {
	each(&[
		(
			"a .git directory in the mount, dirty",
			"rm -rf sub; g clone -q ../sub sub; echo y >> sub/f",
			&[],
		),
		(
			"a .git directory in the mount, new commits",
			"rm -rf sub; g clone -q ../sub sub; echo y >> sub/f; g -C sub commit -q -am moved",
			&[],
		),
		(
			"a .git directory in the mount, clean",
			"rm -rf sub; g clone -q ../sub sub",
			&[],
		),
		(
			"a gitfile naming a repository elsewhere, dirty",
			r#"mv .git/modules/sub ../elsewhere; echo "gitdir: $BASE/elsewhere" > sub/.git
			   g config -f ../elsewhere/config core.worktree "$BASE/super/sub"; echo y >> sub/f"#,
			&[],
		),
		(
			"a gitfile naming a repository elsewhere, no core.worktree",
			r#"mv .git/modules/sub ../elsewhere; echo "gitdir: $BASE/elsewhere" > sub/.git
			   g config -f ../elsewhere/config --unset core.worktree; echo y >> sub/f"#,
			&[],
		),
		(
			"a gitfile naming nothing",
			"echo 'gitdir: ../nowhere' > sub/.git",
			&[],
		),
		(
			"a .git file that isn't a gitfile",
			"rm sub/.git; echo 'not a gitfile' > sub/.git",
			&[],
		),
		(
			"an empty .git directory",
			"rm sub/.git; mkdir sub/.git",
			&[],
		),
		(
			"a module without HEAD",
			"rm .git/modules/sub/HEAD; echo y >> sub/f",
			&[],
		),
		(
			"a module without objects",
			"rm -rf .git/modules/sub/objects; echo y >> sub/f",
			&[],
		),
		(
			"a module with a corrupt index",
			"echo garbage > .git/modules/sub/index; echo y >> sub/f",
			&[],
		),
		(
			"a module with an unparseable config",
			"printf '[core\\n' > .git/modules/sub/config; echo y >> sub/f",
			&[],
		),
	])
	.await;
}

#[tokio::test]
async fn reads_declarations_as_git_does() {
	let declared =
		r#"printf '[submodule "sub"]\n\tpath = sub\n\turl = ../sub\n%b' "$EXTRA" > .gitmodules"#;
	let with = |extra: &str| format!("echo y >> sub/f; EXTRA='{extra}'; {declared}");
	let cases: Vec<(String, String)> = vec![
		(
			"no declaration".into(),
			"g config -f .gitmodules --remove-section submodule.sub; g commit -q -am none; echo y >> sub/f".into(),
		),
		(
			"no declaration, config by path".into(),
			"g config -f .gitmodules --remove-section submodule.sub; g commit -q -am none; g config submodule.sub.ignore all; echo y >> sub/f".into(),
		),
		(
			"no declaration, diff.ignoreSubmodules".into(),
			"g config -f .gitmodules --remove-section submodule.sub; g commit -q -am none; g config diff.ignoreSubmodules all; echo y >> sub/f".into(),
		),
		(
			"renamed, config by name".into(),
			"g config -f .gitmodules --rename-section submodule.sub submodule.named; g commit -q -am renamed; g config submodule.named.ignore all; echo y >> sub/f".into(),
		),
		(
			"renamed, config by path".into(),
			"g config -f .gitmodules --rename-section submodule.sub submodule.named; g commit -q -am renamed; g config submodule.sub.ignore all; echo y >> sub/f".into(),
		),
		(
			"the index's .gitmodules, the work tree's gone".into(),
			"g config -f .gitmodules submodule.sub.ignore all; g add .gitmodules; rm .gitmodules; echo y >> sub/f".into(),
		),
		(
			"HEAD's .gitmodules, the index's and work tree's gone".into(),
			"g config -f .gitmodules submodule.sub.ignore all; g commit -q -am all; g rm -q --cached .gitmodules; rm .gitmodules; echo y >> sub/f".into(),
		),
		(
			"the index's over HEAD's".into(),
			"g config -f .gitmodules submodule.sub.ignore all; g commit -q -am all; g config -f .gitmodules --unset submodule.sub.ignore; g add .gitmodules; rm .gitmodules; echo y >> sub/f".into(),
		),
		(
			"the work tree's over the index's".into(),
			"g config -f .gitmodules submodule.sub.ignore all; g add .gitmodules; g config -f .gitmodules --unset submodule.sub.ignore; echo y >> sub/f".into(),
		),
		(
			// Committed, so the status has no type change for `.gitmodules` itself (gitana's status says
			// ` M` where git says ` T` for a file replaced by a symbolic link, which isn't this probe's).
			"a symlinked .gitmodules".into(),
			"cp .gitmodules ../gm; g config -f ../gm submodule.sub.ignore all; rm .gitmodules; ln -s ../gm .gitmodules; g add .gitmodules; g commit -q -m symlink; echo y >> sub/f".into(),
		),
		("an unparseable .gitmodules".into(), "echo y >> sub/f; printf '[submodule \"sub\"\\n' > .gitmodules".into()),
		("url with no value".into(), with("\\turl\\n")),
		("branch with no value".into(), with("\\tbranch\\n")),
		("update with no value".into(), with("\\tupdate\\n")),
		("ignore with no value".into(), with("\\tignore\\n")),
		("update = bogus".into(), with("\\tupdate = bogus\\n")),
		("fetchRecurseSubmodules = bogus".into(), with("\\tfetchRecurseSubmodules = bogus\\n")),
		("fetchRecurseSubmodules with no value".into(), with("\\tfetchRecurseSubmodules\\n")),
		("shallow = invalid".into(), with("\\tshallow = invalid\\n")),
		("shallow with no value".into(), with("\\tshallow\\n")),
		("active = bogus".into(), with("\\tactive = bogus\\n")),
		("url = -bad".into(), with("\\turl = -bad\\n")),
		("ignore = bogus".into(), with("\\tignore = bogus\\n")),
		("ignore = all".into(), with("\\tignore = all\\n")),
		("two declarations, one path".into(), with("[submodule \"dup\"]\\n\\tpath = sub\\n")),
		("a suspicious name elsewhere".into(), with("[submodule \"../evil\"]\\n\\tpath = other\\n")),
		("another declaration's bad shallow".into(), with("[submodule \"other\"]\\n\\tpath = other\\n\\tshallow = invalid\\n")),
		("another declaration's bad ignore".into(), with("[submodule \"other\"]\\n\\tpath = other\\n\\tignore = bogus\\n")),
		("config submodule.sub.ignore=bogus".into(), "echo y >> sub/f; g config submodule.sub.ignore bogus".into()),
		("config submodule.other.ignore=bogus".into(), "echo y >> sub/f; g config submodule.other.ignore bogus".into()),
	];
	for format in ["sha1", "sha256"] {
		for (label, setup) in &cases {
			Fixture::new(format).check(label, setup, &[]).await;
			if label.contains("shallow = invalid") || label.contains("ignore = bogus") {
				// With the flag given, git doesn't read `.gitmodules` at all.
				Fixture::new(format)
					.check(
						&format!("{label}, flag=none"),
						setup,
						&["--ignore-submodules=none"],
					)
					.await;
			}
		}
	}
}

#[tokio::test]
async fn reads_a_linked_worktree_s_module_store_as_git_does() {
	for format in ["sha1", "sha256"] {
		let fixture = Fixture::new(format);
		fixture
			.check_at(
				"a linked worktree's submodule, dirty",
				"g worktree add -q ../linked; cd ../linked; g submodule update -q --init; echo y >> sub/f",
				"linked",
				&[],
			)
			.await;
	}
}

#[tokio::test]
async fn refuses_the_submodule_s_own_bad_setting_as_git_does() {
	// The submodule's own status refuses a bad `diff.ignoreSubmodules` in its config, so the
	// superproject's fails, unless the setting in effect doesn't look inside (probed vs git 2.55).
	for format in ["sha1", "sha256"] {
		for (setting, set, refusal) in [
			(
				"bad",
				"g -C sub config diff.ignoreSubmodules bogus",
				"bad --ignore-submodules argument: bogus",
			),
			(
				"valueless",
				"printf '[diff]\\n\\tignoreSubmodules\\n' >> .git/modules/sub/config",
				"missing value for 'diff.ignoresubmodules'",
			),
		] {
			for (when, refused) in [
				(None, true),
				(Some("none"), true),
				(Some("untracked"), true),
				(Some("dirty"), false),
				(Some("all"), false),
			] {
				let args: Vec<String> = when
					.map(|when| format!("--ignore-submodules={when}"))
					.into_iter()
					.collect();
				let args: Vec<&str> = args.iter().map(String::as_str).collect();
				let label = format!("the submodule's own {setting} setting, {args:?}");
				let fixture = Fixture::new(format);
				fixture
					.check(&label, &format!("echo x >> sub/f; {set}"), &args)
					.await;
				let ours = fixture
					.gitana_status(&fixture.superproject(), flag(&args))
					.await;
				match ours {
					Err(error) => assert!(
						refused && error.contains(refusal),
						"{label} ({format}): {error}"
					),
					Ok(_) => assert!(!refused, "{label} ({format})"),
				}
			}
		}
	}
}

#[tokio::test]
async fn refuses_a_submodule_with_a_sparse_index() {
	// git reads a sparse index; gitana's status can't, so it refuses where git reports, unless the
	// setting in effect doesn't look inside the submodule. That's a deliberate difference.
	let sparse = "mkdir sub/d; echo d > sub/d/f; g -C sub add -A; g -C sub commit -q -m d; \
		g -C sub sparse-checkout set --cone --sparse-index";
	for format in ["sha1", "sha256"] {
		let fixture = Fixture::new(format);
		fixture.sh(&fixture.superproject(), sparse);
		let git = fixture.git_status(&fixture.superproject(), &[]);
		assert_eq!(
			String::from_utf8_lossy(&git.stdout),
			" M sub\n",
			"{format}: git reads it"
		);
		let error = fixture
			.gitana_status(&fixture.superproject(), None)
			.await
			.unwrap_err();
		assert!(
			error.contains("submodule 'sub'") && error.contains("sparse index"),
			"{format}: {error}"
		);
		// `dirty` doesn't look at the submodule's content, so its status isn't read.
		let fixture = Fixture::new(format);
		fixture
			.check(
				"a sparse index under --ignore-submodules=dirty",
				sparse,
				&["--ignore-submodules=dirty"],
			)
			.await;
	}
}
