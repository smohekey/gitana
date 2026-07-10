//! The host-supplied reflog identity threaded through `update-ref` / `set-symbolic-ref`.
//!
//! The component reads no process env or clock, so a reflog entry is written only when the host
//! passes a `reflog-request` (committer line + message); absent one, the move logs nothing. These
//! tests drive the guest and read the on-disk `logs/` back through the same directory, asserting the
//! exact git reflog lines — including the split-HEAD cascade when the moved branch is HEAD's target.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use anyhow::{Result, anyhow};
use gitana_object::Sha256;
use gitana_repo_host::exports::gitana::repo::porcelain::ReflogRequest;

use self::support::{Session, build_fixture};

/// A reflog committer line (`Name <email> seconds ±hhmm`), the identity the host credits.
const REFLOG_WHO: &str = "R E Flog <reflog@example.com> 1719901000 +0000";

/// Moving a branch that `HEAD` points at with a reflog request writes the branch's `logs/` entry and
/// mirrors it into `logs/HEAD` (git's split-HEAD update) — both crediting the host-supplied committer.
#[tokio::test]
async fn update_ref_writes_reflog_and_cascades_to_head() -> Result<()> {
	let fixture = build_fixture::<Sha256>().await?;
	let git_dir = fixture.dir.path();
	let mut session = Session::open(git_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// HEAD points at main (init), which the fixture set to M. Move it to feature's tip D, logging.
	let request = ReflogRequest {
		committer: REFLOG_WHO.to_owned(),
		message: "update-ref: move main to feature".to_owned(),
	};
	porcelain
		.call_update_ref(
			&mut *store,
			handle,
			"refs/heads/main",
			&fixture.d,
			Some(&fixture.m),
			Some(&request),
		)
		.await?
		.map_err(|error| anyhow!("update-ref: {error:?}"))?;

	let expected = format!(
		"{} {} {}\t{}\n",
		fixture.m, fixture.d, REFLOG_WHO, request.message
	);
	// The branch's own reflog…
	assert_eq!(
		std::fs::read_to_string(git_dir.join("logs/refs/heads/main"))?,
		expected
	);
	// …and the split-HEAD cascade: HEAD points at main, so git mirrors the same line into logs/HEAD.
	assert_eq!(
		std::fs::read_to_string(git_dir.join("logs/HEAD"))?,
		expected
	);

	Ok(())
}

/// Without a reflog request the move is a pure plumbing update: no `logs/` entry is written, even for
/// a standard-logged branch namespace.
#[tokio::test]
async fn update_ref_without_reflog_writes_nothing() -> Result<()> {
	let fixture = build_fixture::<Sha256>().await?;
	let git_dir = fixture.dir.path();
	let mut session = Session::open(git_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// Create a fresh branch with no reflog request.
	porcelain
		.call_update_ref(
			&mut *store,
			handle,
			"refs/heads/quiet",
			&fixture.d,
			None,
			None,
		)
		.await?
		.map_err(|error| anyhow!("update-ref: {error:?}"))?;

	assert_eq!(
		fixture.d,
		std::fs::read_to_string(git_dir.join("refs/heads/quiet"))?.trim(),
		"the ref itself still moved"
	);
	assert!(
		!git_dir.join("logs/refs/heads/quiet").exists(),
		"no reflog request means no reflog entry"
	);

	Ok(())
}

/// Retargeting `HEAD` with a reflog request appends `HEAD`'s reflog (from its old resolved value to
/// the new target's), crediting the host committer.
#[tokio::test]
async fn set_symbolic_ref_writes_head_reflog() -> Result<()> {
	let fixture = build_fixture::<Sha256>().await?;
	let git_dir = fixture.dir.path();
	let mut session = Session::open(git_dir).await?;
	let porcelain = session.repo.gitana_repo_porcelain().repository();
	let store = &mut session.store;
	let handle = session.handle;

	// HEAD → main (→ M); retarget it at feature (→ D), logging the object movement.
	let request = ReflogRequest {
		committer: REFLOG_WHO.to_owned(),
		message: "checkout: moving to feature".to_owned(),
	};
	porcelain
		.call_set_symbolic_ref(
			&mut *store,
			handle,
			"HEAD",
			"refs/heads/feature",
			Some(&request),
		)
		.await?
		.map_err(|error| anyhow!("set-symbolic-ref: {error:?}"))?;

	assert_eq!(
		std::fs::read_to_string(git_dir.join("logs/HEAD"))?,
		format!(
			"{} {} {}\t{}\n",
			fixture.m, fixture.d, REFLOG_WHO, request.message
		)
	);

	Ok(())
}
