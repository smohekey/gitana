//! `gta push` — send the current branch to the configured origin over Git Smart
//! HTTP. With `--signed`, attach a push certificate. `--force` permits a
//! non-fast-forward update, and `--delete <branch>` sends a delete ref command.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Backend;
use anyhow::Result;
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::PushOutcome;
use gitana_remote::{self as transport, Origin, ReqwestTransport};
use gitana_repository::Repository;

use crate::dispatch;
use crate::repo;
use crate::signer::LazyCliSigner;

/// Push `HEAD`'s branch to the origin. `signed` attaches a push certificate (signed with
/// `--signing-key`, or git config `user.signingkey`); `force` permits a non-fast-forward update;
/// `delete` removes a remote branch instead of pushing.
pub async fn run(
	cwd: &Path,
	signed: bool,
	signing_key: Option<PathBuf>,
	force: bool,
	delete: Option<String>,
) -> Result<()> {
	let found = repo::discover(cwd)?;
	let origin = Origin::load(&found.common_dir)?;
	let http = ReqwestTransport::new();
	let body = transport::fetch_advertisement(&http, &origin, "git-receive-pack").await?;

	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	match local {
		HashKind::Sha1 => {
			push_into::<Sha1>(
				&http,
				&origin,
				&found,
				&body,
				signed,
				signing_key,
				force,
				delete,
				cwd,
			)
			.await
		}
		HashKind::Sha256 => {
			push_into::<Sha256>(
				&http,
				&origin,
				&found,
				&body,
				signed,
				signing_key,
				force,
				delete,
				cwd,
			)
			.await
		}
	}
}

#[allow(clippy::too_many_arguments)]
async fn push_into<H: HashAlgorithm>(
	http: &ReqwestTransport,
	origin: &Origin,
	found: &repo::Discovered,
	body: &[u8],
	signed: bool,
	signing_key: Option<PathBuf>,
	force: bool,
	delete: Option<String>,
	cwd: &Path,
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir)?;
	// A signed push certificate is signed like an explicit `commit -S`: an unset `gpg.format` is
	// assumed `ssh` (rather than rejected as config-driven signing is), and the key is resolved lazily
	// so a "server does not accept signed pushes" error is not masked by a missing signing key.
	// `--signed --delete` attaches a signed delete certificate, so a `require` server can authorise it.
	let outcome = if signed {
		let signer = LazyCliSigner::new(&repository, signing_key, cwd.to_path_buf(), false);
		gitana_porcelain::push_signed(
			http,
			&repository,
			origin,
			body,
			force,
			delete,
			async || pusher_ident(&repository).await,
			&signer,
		)
		.await?
	} else {
		gitana_porcelain::push(http, &repository, origin, body, force, delete).await?
	};

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
