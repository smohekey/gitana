//! `gta push` — send refs to the configured origin over Git Smart HTTP. Positional refspecs
//! (`[+]<src>:<dst>`, `<name>`, or `:<dst>` to delete) select what to push; with none, `HEAD`'s branch
//! (or `remote.origin.push`) is pushed to the same-name remote branch. `--signed` attaches a push
//! certificate, `--force` permits a non-fast-forward, and `--delete <ref>` is sugar for a `:<ref>`
//! deletion.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Backend;
use anyhow::Result;
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::PushTags;
use gitana_remote::{self as transport, Origin, PushRefspec, ReqwestTransport};
use gitana_repository::Repository;

use crate::dispatch;
use crate::repo;
use crate::signer::LazyCliSigner;

/// Push to the origin. `repository` (if given) must name the `origin` remote; `refspecs` and `delete`
/// select what to push; `all_tags` (`--tags`) / `follow_tags` (`--follow-tags`) add tags; `signed`
/// attaches a push certificate (signed with `--signing-key`, or git config `user.signingkey`); `force`
/// permits a non-fast-forward update.
#[allow(clippy::too_many_arguments)]
pub async fn run(
	cwd: &Path,
	repository: Option<String>,
	refspecs: Vec<String>,
	signed: bool,
	signing_key: Option<PathBuf>,
	force: bool,
	delete: Option<String>,
	all_tags: bool,
	follow_tags: bool,
) -> Result<()> {
	let tags = if all_tags {
		PushTags::All
	} else if follow_tags {
		PushTags::Follow
	} else {
		PushTags::None
	};
	// git puts the remote first (`push [<remote>] [<refspec>...]`); gitana has exactly one remote
	// (`origin`), so a leading positional is the remote only when it *is* `origin` — otherwise it is a
	// refspec, and `gta push HEAD:refs/heads/x` works without redundantly naming origin.
	let mut spec_texts = Vec::new();
	match repository {
		Some(remote) if remote == "origin" => {}
		Some(refspec) => spec_texts.push(refspec),
		None => {}
	}
	spec_texts.extend(refspecs);
	// Parse the refspecs, plus `--delete <ref>` as a `:<ref>` deletion. An empty list lets the
	// porcelain default to `remote.origin.push` or `HEAD`'s branch.
	let mut specs = spec_texts
		.iter()
		.map(|s| PushRefspec::parse(s))
		.collect::<Result<Vec<_>>>()?;
	if let Some(target) = delete {
		specs.push(PushRefspec::parse(&format!(":{target}"))?);
	}

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
				specs,
				signed,
				signing_key,
				force,
				tags,
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
				specs,
				signed,
				signing_key,
				force,
				tags,
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
	refspecs: Vec<PushRefspec>,
	signed: bool,
	signing_key: Option<PathBuf>,
	force: bool,
	tags: PushTags,
	cwd: &Path,
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir)?;
	// A signed push certificate is signed like `commit -S`: the format follows `gpg.format` (unset →
	// OpenPGP, git's default), and the key is resolved lazily so a "server does not accept signed
	// pushes" error is not masked by a missing signing key.
	let outcome = if signed {
		let signer = LazyCliSigner::new(&repository, signing_key, cwd.to_path_buf());
		gitana_porcelain::push_signed(
			http,
			&repository,
			origin,
			body,
			force,
			refspecs,
			tags,
			async || pusher_ident(&repository).await,
			&signer,
		)
		.await?
	} else {
		gitana_porcelain::push(http, &repository, origin, body, force, refspecs, tags).await?
	};

	if outcome.is_up_to_date() {
		println!("Everything up-to-date");
		return Ok(());
	}
	for result in &outcome.results {
		if result.deleted {
			println!("Deleted {} on {}", result.refname, origin.url);
		} else {
			let how = if outcome.signed {
				" (signed)"
			} else if result.forced {
				" (forced)"
			} else {
				""
			};
			println!("Pushed {} -> {}{how}", result.refname, origin.url);
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
