//! Shared git-oracle test harness: build fixtures with stock `git`, run our read fns, assert.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use gitana_linked_worktree::{RepositoryId, WorktreeContext};
use gitana_object::HashKind;

/// git's automatic maintenance, off for every command these fixtures run. After a commit, git (2.55)
/// starts `git maintenance run --auto --detach`, which carries on in the background in the very
/// repository a test is still working in — its default `worktree-prune` task deletes a linked worktree's
/// half-built admin folder, and other tasks repack under a running test. `gc.auto=0` covers older gits,
/// which ran `gc --auto` straight after a commit. Passed per command rather than written into each
/// repository's config, so repositories a test makes itself (`git clone`, a plain `git init`) are covered.
const QUIET_MAINTENANCE: [&str; 4] = ["-c", "maintenance.auto=false", "-c", "gc.auto=0"];

/// A `git` command that behaves the same on every machine, with git's automatic maintenance off (see
/// [`QUIET_MAINTENANCE`]). Every git these fixtures run goes through it.
///
/// The fixtures build repositories with stock git and check gitana's results against it, so nothing
/// from the machine may reach that git:
/// - No global or system config file (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`). A
///   developer's `core.hooksPath`, `init.defaultRefFormat`, `index.skipHash` or
///   `status.showUntrackedFiles` otherwise changes what the fixtures build or what git reports.
/// - No default global ignore or attributes file (`XDG_CONFIG_HOME=/dev/null`): git reads
///   `~/.config/git/ignore` and `attributes` even with no global config.
/// - No inherited `GIT_*` variable but `GIT_TRACE*`. git exports `GIT_DIR` and friends to its hooks,
///   and an inherited `GIT_DIR` beats `-C`, so a suite run from a hook would commit into the outer
///   repository; `GIT_CONFIG_PARAMETERS` and `GIT_CONFIG_COUNT` would add config. The trace variables
///   stay, so what the fixtures' git does can still be traced.
pub fn git_command() -> Command {
	let mut command = Command::new("git");
	for (name, _) in std::env::vars_os() {
		let spelled = name.to_string_lossy();
		if spelled.starts_with("GIT_") && !spelled.starts_with("GIT_TRACE") {
			command.env_remove(&name);
		}
	}
	command
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("XDG_CONFIG_HOME", "/dev/null")
		.args(QUIET_MAINTENANCE);
	command
}

/// Run `git` with `args`, asserting success, returning stdout.
pub fn git(args: &[&str]) -> String {
	let out = git_command().args(args).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

/// Run `git` with `args`, tolerating a non-zero exit (e.g. a conflicting merge), returning stdout.
pub fn git_try(args: &[&str]) -> String {
	let out = git_command().args(args).output().expect("run git");
	String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run `git` with `args`, returning whether it exited successfully (for oracle refusal checks and
/// probing optional subcommands like `worktree add --orphan`).
pub fn git_ok(args: &[&str]) -> bool {
	git_command()
		.args(args)
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}

/// A fresh, empty temp directory unique to this call — tag + process + a monotonic counter, so
/// concurrently-running tests (and repeated calls within one test, e.g. the sha256 probe) never collide.
pub fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static COUNTER: AtomicU64 = AtomicU64::new(0);
	let n = COUNTER.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gitana-lw-{tag}-{}-{n}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// Whether the local `git` can init a sha256 repository.
pub fn git_supports_sha256() -> bool {
	let probe = unique_tmp("probe-sha256");
	let ok = git_command()
		.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	let _ = std::fs::remove_dir_all(&probe);
	ok
}

/// The object formats to exercise: always sha1, plus sha256 when git supports it.
pub fn formats() -> Vec<(&'static str, HashKind)> {
	let mut out = vec![("sha1", HashKind::Sha1)];
	if git_supports_sha256() {
		out.push(("sha256", HashKind::Sha256));
	} else {
		eprintln!("note: skipping sha256 cases (git lacks --object-format=sha256)");
	}
	out
}

/// `git init --object-format=<fmt>` an ordinary repo at `work` with a committer identity set. `-b
/// main` pins the initial branch so tests do not depend on the ambient `init.defaultBranch` (which
/// differs by platform — `master` on stock git, `main` on some vendored builds).
pub fn init_repo(work: &Path, fmt: &str) {
	let w = work.to_str().unwrap();
	git(&[
		"init",
		"-b",
		"main",
		&format!("--object-format={fmt}"),
		"-q",
		w,
	]);
	git(&["-C", w, "config", "user.name", "T"]);
	git(&["-C", w, "config", "user.email", "t@e"]);
	git(&["-C", w, "config", "commit.gpgsign", "false"]);
}

/// `git init --bare --object-format=<fmt>` a bare repo at `git_dir` (initial branch pinned to `main`;
/// see [`init_repo`]).
pub fn init_bare(git_dir: &Path, fmt: &str) {
	let g = git_dir.to_str().unwrap();
	git(&[
		"init",
		"--bare",
		"-b",
		"main",
		&format!("--object-format={fmt}"),
		"-q",
		g,
	]);
	git(&["-C", g, "config", "user.name", "T"]);
	git(&["-C", g, "config", "user.email", "t@e"]);
	git(&["-C", g, "config", "commit.gpgsign", "false"]);
}

/// Write `name`=`content` under `work`, stage + commit it, and return the new `HEAD` hex.
pub fn commit_file(work: &Path, name: &str, content: &str, msg: &str) -> String {
	let w = work.to_str().unwrap();
	std::fs::write(work.join(name), content).unwrap();
	git(&["-C", w, "add", name]);
	git(&["-C", w, "commit", "-q", "-m", msg]);
	git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned()
}

/// Resolve `path` to its real form (temp dirs are often symlinked, e.g. macOS `/var` → `/private/var`).
pub fn canonical(path: &Path) -> PathBuf {
	std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A `RepositoryId` anchored on `<work>/.git`.
pub fn rid_at(work: &Path) -> RepositoryId {
	RepositoryId::at_common_dir(canonical(&work.join(".git"))).unwrap()
}

/// A `RepositoryId` anchored on a bare repo's git dir.
pub fn rid_bare(git_dir: &Path) -> RepositoryId {
	RepositoryId::at_common_dir(canonical(git_dir)).unwrap()
}

/// A local-config-only `WorktreeContext` over `<work>/.git` — the default an embedding consumer gets.
pub fn ctx_at(work: &Path) -> WorktreeContext {
	WorktreeContext::new(rid_at(work))
}

/// A local-config-only `WorktreeContext` over a bare repo's git dir.
pub fn ctx_bare(git_dir: &Path) -> WorktreeContext {
	WorktreeContext::new(rid_bare(git_dir))
}
