//! The component include-expands `[include]`/`includeIf` **once at open**, installing the result as the
//! repository's effective config so every consumer (`repack` → `pack.packSizeLimit`, `fetch` →
//! `remote.origin.fetch`/`tagOpt`, ref writes → `core.logAllRefUpdates`) honours included values. A
//! structurally bad include (cycle, paradox, directory target) aborts the open, as git aborts a command
//! that reads a bad config; a bad *value* surfaces at its consumer (a malformed `pack.packSizeLimit`
//! fails `repack`).
//!
//! The component holds only a path-less git-dir descriptor, so include targets resolve *relative to the
//! store root* and there is no global/system/`-c` layer, no `$PWD`, and no gitdir path. These tests
//! prove, over that capability: an included value reaches `repack` (a *malformed* `pack.packSizeLimit`
//! set only via an include makes `repack` fail, where a raw read — which sees the `[include]` directive,
//! not the included value — would not); `onbranch:` is evaluated from HEAD; `hasconfig:` reads the
//! included file (its paradox fires); a cycle aborts open; git's filesystem `..` (missing-prefix skip,
//! in-root resolve) and directory-target semantics; and the common-dir resolution for a linked worktree.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::path::Path;

use anyhow::Result;
use gitana_object::Sha256;
use gitana_repo_host::exports::gitana::repo::porcelain::RepoError;

use self::support::{Session, build_fixture, native_repo};

/// Append `extra` to the repository's local `config` (which the fixture/init already wrote).
fn append_config(git_dir: &Path, extra: &str) {
	let path = git_dir.join("config");
	let mut content = std::fs::read_to_string(&path).expect("read config");
	if !content.ends_with('\n') {
		content.push('\n');
	}
	content.push_str(extra);
	std::fs::write(&path, content).expect("write config");
}

/// A minimal, freshly-initialised sha256 repository (valid config so the guest detects the format, but
/// no objects) — enough for the open-time-expansion failure cases, which abort before any object work.
async fn minimal_repo() -> Result<tempfile::TempDir> {
	let dir = tempfile::tempdir()?;
	native_repo::<Sha256>(dir.path())?.init().await?;
	Ok(dir)
}

/// Repack the opened repository, returning the guest's typed result (the malformed-config path surfaces
/// its error here).
async fn repack(session: &mut Session) -> Result<Result<(), RepoError>> {
	let outcome = session
		.repo
		.gitana_repo_porcelain()
		.repository()
		.call_repack(&mut session.store, session.handle, false)
		.await?;
	Ok(outcome.map(|_report| ()))
}

#[tokio::test]
async fn included_pack_size_limit_reaches_repack() -> Result<()> {
	// A: no include — the raw local config has no `pack.packSizeLimit`, so repack succeeds.
	let baseline = build_fixture::<Sha256>().await?;
	let mut session = Session::open(baseline.dir.path()).await?;
	assert!(
		repack(&mut session).await?.is_ok(),
		"repack should succeed with no pack.packSizeLimit configured"
	);

	// B: a *malformed* `pack.packSizeLimit` set only via an include. Open still succeeds (the include is
	// valid); repack fails because `pack_size_limit()` reads the effective (include-expanded) config —
	// proving the included value was installed and consumed. A raw local read would see only the
	// `[include]` directive, not the included `packSizeLimit`, and repack would wrongly succeed.
	let fixture = build_fixture::<Sha256>().await?;
	std::fs::write(
		fixture.dir.path().join("pack.cfg"),
		"[pack]\n\tpackSizeLimit = notanumber\n",
	)?;
	append_config(fixture.dir.path(), "[include]\n\tpath = pack.cfg\n");
	let mut session = Session::open(fixture.dir.path()).await?;
	assert!(
		repack(&mut session).await?.is_err(),
		"repack ignored an included malformed pack.packSizeLimit — effective config was not expanded"
	);
	Ok(())
}

#[tokio::test]
async fn onbranch_include_is_evaluated_from_head() -> Result<()> {
	// HEAD on `feature/x`: an `includeIf onbranch:feature/*` sets a malformed packSizeLimit, so repack
	// fails only because the branch matched — proving HEAD was read to resolve the branch.
	let matched = build_fixture::<Sha256>().await?;
	std::fs::write(
		matched.dir.path().join("HEAD"),
		"ref: refs/heads/feature/x\n",
	)?;
	std::fs::write(
		matched.dir.path().join("br.cfg"),
		"[pack]\n\tpackSizeLimit = notanumber\n",
	)?;
	append_config(
		matched.dir.path(),
		"[includeIf \"onbranch:feature/*\"]\n\tpath = br.cfg\n",
	);
	let mut session = Session::open(matched.dir.path()).await?;
	assert!(
		repack(&mut session).await?.is_err(),
		"onbranch:feature/* should have matched HEAD and applied the include"
	);

	// HEAD on `main`: the same condition does not match, so the include is not applied and repack works.
	let unmatched = build_fixture::<Sha256>().await?;
	std::fs::write(unmatched.dir.path().join("HEAD"), "ref: refs/heads/main\n")?;
	std::fs::write(
		unmatched.dir.path().join("br.cfg"),
		"[pack]\n\tpackSizeLimit = notanumber\n",
	)?;
	append_config(
		unmatched.dir.path(),
		"[includeIf \"onbranch:feature/*\"]\n\tpath = br.cfg\n",
	);
	let mut session = Session::open(unmatched.dir.path()).await?;
	assert!(
		repack(&mut session).await?.is_ok(),
		"onbranch:feature/* must not match `main`; the include should not apply"
	);
	Ok(())
}

#[tokio::test]
async fn include_cycle_aborts_open() -> Result<()> {
	// Config is include-expanded once at open (installed as the effective config), so a self-referential
	// include's depth cap aborts the open — a structurally bad config, like git aborting a command that
	// reads it.
	let dir = minimal_repo().await?;
	std::fs::write(
		dir.path().join("loop.cfg"),
		"[include]\n\tpath = loop.cfg\n",
	)?;
	append_config(dir.path(), "[include]\n\tpath = loop.cfg\n");
	assert!(
		Session::open(dir.path()).await.is_err(),
		"an include cycle should abort open (depth cap)"
	);
	Ok(())
}

#[tokio::test]
async fn hasconfig_paradox_aborts_open() -> Result<()> {
	// A file pulled in by a `hasconfig:remote.*.url:` include may not itself set a remote URL. The
	// forced-true pre-scan (run at open) reads the included file, sees the forbidden URL, and — with a
	// hasconfig directive present — aborts. Exercises the pre-scan + resolver + paradox guard.
	let dir = minimal_repo().await?;
	std::fs::write(
		dir.path().join("id.cfg"),
		"[remote \"x\"]\n\turl = https://example.test/x.git\n",
	)?;
	append_config(
		dir.path(),
		concat!(
			"[remote \"o\"]\n\turl = https://example.test/o.git\n",
			"[includeIf \"hasconfig:remote.*.url:https://**\"]\n\tpath = id.cfg\n",
		),
	);
	assert!(
		Session::open(dir.path()).await.is_err(),
		"the hasconfig remote-URL paradox should abort open"
	);
	Ok(())
}

#[tokio::test]
async fn dotdot_through_missing_prefix_is_skipped() -> Result<()> {
	// git skips a `..` include whose preceding directory does not exist (filesystem ENOENT). `missing`
	// is absent, so `missing/../two.cfg` is skipped even though `two.cfg` (malformed) exists — repack ok.
	let fixture = build_fixture::<Sha256>().await?;
	let git = fixture.dir.path();
	std::fs::write(
		git.join("two.cfg"),
		"[pack]\n\tpackSizeLimit = notanumber\n",
	)?;
	append_config(git, "[include]\n\tpath = missing/../two.cfg\n");
	let mut session = Session::open(git).await?;
	assert!(
		repack(&mut session).await?.is_ok(),
		"a `..` through a missing prefix should be skipped, as git skips ENOENT"
	);
	Ok(())
}

#[tokio::test]
async fn empty_include_value_resolves_to_directory_and_aborts_open() -> Result<()> {
	// An explicit empty include value (`[include] path =`) resolves to the config directory itself, a
	// directory target; git fatals ("Is a directory"), so the component aborts the open rather than
	// silently ignoring the malformed include.
	let dir = minimal_repo().await?;
	append_config(dir.path(), "[include]\n\tpath = \n");
	assert!(
		Session::open(dir.path()).await.is_err(),
		"an empty include value should abort open (resolves to a directory)"
	);
	Ok(())
}

#[tokio::test]
async fn directory_include_target_aborts_open() -> Result<()> {
	// git fatals on a directory include target ("unable to access … Is a directory"); the component
	// likewise aborts (open fails at expansion) rather than silently skipping the directory as absent.
	let dir = minimal_repo().await?;
	std::fs::create_dir_all(dir.path().join("adir"))?;
	append_config(dir.path(), "[include]\n\tpath = adir\n");
	assert!(
		Session::open(dir.path()).await.is_err(),
		"a directory include target should abort open, as git fatals"
	);
	Ok(())
}

#[tokio::test]
async fn in_root_parent_include_is_resolved() -> Result<()> {
	// A nested include using `../` that stays inside the store root must be read, not skipped: the top
	// config includes `sub/one.cfg`, which includes `../two.cfg` (→ `sub/../two.cfg` → `two.cfg`). The
	// leaf sets a malformed packSizeLimit, so repack fails only if the in-root `..` include was resolved.
	let fixture = build_fixture::<Sha256>().await?;
	let git = fixture.dir.path();
	std::fs::create_dir_all(git.join("sub"))?;
	std::fs::write(git.join("sub/one.cfg"), "[include]\n\tpath = ../two.cfg\n")?;
	std::fs::write(
		git.join("two.cfg"),
		"[pack]\n\tpackSizeLimit = notanumber\n",
	)?;
	append_config(git, "[include]\n\tpath = sub/one.cfg\n");
	let mut session = Session::open(git).await?;
	assert!(
		repack(&mut session).await?.is_err(),
		"an in-root `../` include (sub/../two.cfg) should be resolved, not skipped"
	);
	Ok(())
}

#[tokio::test]
async fn linked_worktree_resolves_includes_from_common_dir() -> Result<()> {
	// The shared config includes a file named like a per-worktree file (`config.worktree`); it lives in
	// the COMMON dir and sets a malformed packSizeLimit. Opening a linked worktree (a distinct per-worktree
	// git dir) must resolve the include against the common dir — repack fails — rather than routing it to
	// the per-worktree git dir, where it is absent and would be wrongly skipped.
	let fixture = build_fixture::<Sha256>().await?;
	let common = fixture.dir.path();
	std::fs::write(
		common.join("config.worktree"),
		"[pack]\n\tpackSizeLimit = notanumber\n",
	)?;
	append_config(common, "[include]\n\tpath = config.worktree\n");

	let worktree = tempfile::tempdir()?;
	std::fs::write(worktree.path().join("HEAD"), "ref: refs/heads/feature\n")?;
	let work = tempfile::tempdir()?;
	let mut session = Session::open_worktree(worktree.path(), common, work.path()).await?;
	assert!(
		repack(&mut session).await?.is_err(),
		"a linked worktree must resolve config includes from the common dir, not the per-worktree git dir"
	);
	Ok(())
}

#[tokio::test]
async fn benign_relative_include_opens_cleanly() -> Result<()> {
	// A plain relative include that sets a valid value: open succeeds and repack (which reads the
	// included, valid packSizeLimit) succeeds — the include is read and spliced without disturbing a
	// normal open. Confirms expansion does not break the common case.
	let fixture = build_fixture::<Sha256>().await?;
	std::fs::write(
		fixture.dir.path().join("ok.cfg"),
		"[pack]\n\tpackSizeLimit = 2097152\n",
	)?;
	append_config(fixture.dir.path(), "[include]\n\tpath = ok.cfg\n");
	let mut session = Session::open(fixture.dir.path()).await?;
	assert!(
		repack(&mut session).await?.is_ok(),
		"a benign relative include should open and operate normally"
	);
	Ok(())
}
