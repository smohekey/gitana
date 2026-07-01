//! `gta push` — send the current branch to the configured origin over Git Smart
//! HTTP. With `--signed`, attach a push certificate. `--force` permits a
//! non-fast-forward update, and `--delete <branch>` sends a delete ref command.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Backend;
use anyhow::Result;
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::PushOutcome;
use gitana_remote::{self as transport, Origin};
use gitana_repository::Repository;

use crate::dispatch;
use crate::repo;

/// Push `HEAD`'s branch to the origin. `signed` attaches a push certificate; `force`
/// permits a non-fast-forward update; `delete` removes a remote branch instead of
/// pushing.
pub async fn run(
	cwd: &Path,
	signed: bool,
	_signing_key: Option<PathBuf>,
	force: bool,
	delete: Option<String>,
) -> Result<()> {
	let found = repo::discover(cwd)?;
	let origin = Origin::load(&found.common_dir)?;
	let body = transport::fetch_advertisement(&origin, "git-receive-pack").await?;

	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	match local {
		HashKind::Sha1 => push_into::<Sha1>(&origin, &found, &body, signed, force, delete).await,
		HashKind::Sha256 => push_into::<Sha256>(&origin, &found, &body, signed, force, delete).await,
	}
}

async fn push_into<H: HashAlgorithm>(
	origin: &Origin,
	found: &repo::Discovered,
	body: &[u8],
	signed: bool,
	force: bool,
	delete: Option<String>,
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir);
	let outcome = gitana_porcelain::push(
		&repository,
		origin,
		body,
		force,
		delete,
		signed,
		async || pusher_ident(&repository).await,
	)
	.await?;

	match outcome {
		PushOutcome::Deleted { refname } => println!("Deleted {refname} on {}", origin.url),
		PushOutcome::UpToDate => println!("Everything up-to-date"),
		PushOutcome::Pushed {
			branch,
			signed,
			forced,
		} => {
			let how = match (signed, forced) {
				(true, _) => " (signed)",
				(false, true) => " (forced)",
				_ => "",
			};
			println!("Pushed {branch} -> {}{how}", origin.url);
		}
	}
	Ok(())
}

/// The pusher identity for a certificate: `Name <email> <unix-ts> +0000`. Always stamped with the
/// push time, so unlike a commit it ignores any `GIT_AUTHOR_DATE`.
async fn pusher_ident<H: HashAlgorithm>(repo: &Repository<Backend, H>) -> Result<String> {
	let config = repo.read_config().await.ok();
	let secs = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0);
	gitana_identity::signature(
		"AUTHOR",
		std::env::var("GIT_AUTHOR_NAME").ok(),
		std::env::var("GIT_AUTHOR_EMAIL").ok(),
		config.as_ref(),
		&format!("{secs} +0000"),
	)
}
