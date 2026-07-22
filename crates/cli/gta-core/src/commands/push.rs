//! `gta push` — send refs to the configured origin over Git Smart HTTP or SSH. Positional refspecs
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
use gitana_remote::{
	self as transport, Connection, HttpConnection, PushRefspec, RemoteUrl, SshConnection,
};
use gitana_repository::Repository;

use crate::{git_config, transport_for, url_rewrite};

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
	atomic: bool,
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

	let found = repo::discover(cwd).await?;
	// git's push-URL selection: `remote.origin.pushurl` (with `insteadOf`) if set, else
	// `remote.origin.url` with `pushInsteadOf` (falling back to `insteadOf`) — over the merged config.
	let config = git_config::effective_config_at(&found.git_dir, &found.common_dir).await?;
	let url = url_rewrite::resolve_push_url(&config, "origin")?;
	let remote = RemoteUrl::parse(&url)?;
	// A credential-free form for display and the push certificate's pushee — *all* userinfo stripped (a
	// token can occupy the username field), so no credential reaches a print or a signed certificate. The
	// raw `url` is only for the auth-bearing transport parse above.
	let display = transport::anonymize_url(&url);
	// A relative askpass (HTTP) / `GIT_SSH_COMMAND` (SSH) resolves against the worktree root, as git runs
	// it from there (bare: git dir).
	let askpass_cwd = found
		.worktree_root
		.clone()
		.unwrap_or_else(|| found.common_dir.clone());

	// Open the receive-pack connection (an HTTP POST endpoint, or the SSH stateful stream), then run the
	// push over it — one path for both transports.
	match remote {
		RemoteUrl::Http(origin) => {
			let http = transport_for(config, &origin, askpass_cwd)?;
			let body = transport::fetch_advertisement(&http, &origin, "git-receive-pack").await?;
			// The connection's own advertisement is unused (push takes the advertisement as an argument),
			// so an empty one suffices.
			let mut connection = HttpConnection::new(
				&http,
				origin.receive_pack(),
				transport::RECEIVE_PACK_REQUEST,
				Vec::new(),
			);
			push_dispatch(
				&mut connection,
				&found,
				&body,
				&display,
				specs,
				signed,
				signing_key,
				force,
				atomic,
				tags,
				cwd,
			)
			.await
		}
		RemoteUrl::Ssh(ssh) => {
			let mut connection = SshConnection::open(&ssh, "git-receive-pack", &askpass_cwd).await?;
			let body = connection.advertisement().to_vec();
			push_dispatch(
				&mut connection,
				&found,
				&body,
				&display,
				specs,
				signed,
				signing_key,
				force,
				atomic,
				tags,
				cwd,
			)
			.await
		}
	}
}

/// Negotiate the object format from the advertisement, then run the per-hash push over `connection`.
#[allow(clippy::too_many_arguments)]
async fn push_dispatch(
	connection: &mut impl Connection,
	found: &repo::RepositoryLayout,
	body: &[u8],
	url: &str,
	specs: Vec<PushRefspec>,
	signed: bool,
	signing_key: Option<PathBuf>,
	force: bool,
	atomic: bool,
	tags: PushTags,
	cwd: &Path,
) -> Result<()> {
	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(body)?)?;
	match local {
		HashKind::Sha1 => {
			push_into::<Sha1>(
				connection,
				found,
				body,
				url,
				specs,
				signed,
				signing_key,
				force,
				atomic,
				tags,
				cwd,
			)
			.await
		}
		HashKind::Sha256 => {
			push_into::<Sha256>(
				connection,
				found,
				body,
				url,
				specs,
				signed,
				signing_key,
				force,
				atomic,
				tags,
				cwd,
			)
			.await
		}
	}
}

#[allow(clippy::too_many_arguments)]
async fn push_into<H: HashAlgorithm>(
	connection: &mut impl Connection,
	found: &repo::RepositoryLayout,
	body: &[u8],
	url: &str,
	refspecs: Vec<PushRefspec>,
	signed: bool,
	signing_key: Option<PathBuf>,
	force: bool,
	atomic: bool,
	tags: PushTags,
	cwd: &Path,
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir).await?;
	// A signed push certificate is signed like `commit -S`: the format follows `gpg.format` (unset →
	// OpenPGP, git's default), and the key is resolved lazily so a "server does not accept signed
	// pushes" error is not masked by a missing signing key. The certificate's pushee is the push URL.
	let outcome = if signed {
		let signer = LazyCliSigner::new(&repository, signing_key, cwd.to_path_buf());
		gitana_porcelain::push_signed(
			connection,
			&repository,
			url,
			body,
			force,
			atomic,
			refspecs,
			tags,
			async || pusher_ident(&repository).await,
			&signer,
		)
		.await?
	} else {
		gitana_porcelain::push(connection, &repository, body, force, atomic, refspecs, tags).await?
	};

	if outcome.is_up_to_date() {
		println!("Everything up-to-date");
		return Ok(());
	}
	for result in &outcome.results {
		if result.deleted {
			println!("Deleted {} on {}", result.refname, url);
		} else {
			let how = if outcome.signed {
				" (signed)"
			} else if result.forced {
				" (forced)"
			} else {
				""
			};
			println!("Pushed {} -> {}{how}", result.refname, url);
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
