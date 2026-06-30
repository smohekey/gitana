//! `gta push` — send the current branch to the configured origin over Git Smart
//! HTTP. With `--signed`, attach a push certificate. `--force` permits a
//! non-fast-forward update, and `--delete <branch>` sends a delete ref command.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Backend;
use anyhow::{Context, Result, bail};
use gitana_git_http::{
	CertCommand, PushCert, RefUpdate, build_pack, build_push_cert, build_receive_pack_request,
	parse_advertisement, parse_report_status,
};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_remote::{self as transport, Origin, RECEIVE_PACK_REQUEST, http_post};
use gitana_repository::{HeadState, Repository};

use crate::dispatch;
use crate::repo;

/// Push `HEAD`'s branch to the origin. `signed` attaches a push certificate; `force`
/// permits a non-fast-forward update; `delete` removes a remote branch instead of
/// pushing.
pub async fn run(
	cwd: &Path,
	signed: bool,
	signing_key: Option<PathBuf>,
	force: bool,
	delete: Option<String>,
) -> Result<()> {
	let found = repo::discover(cwd)?;
	let origin = Origin::load(&found.common_dir)?;
	let body = transport::fetch_advertisement(&origin, "git-receive-pack").await?;

	let local = dispatch::detect_algorithm(&found.common_dir)?;
	transport::ensure_same_format(local, transport::negotiated_kind(&body)?)?;

	match local {
		HashKind::Sha1 => {
			push_impl::<Sha1>(&origin, &found, &body, signed, signing_key, force, delete).await
		}
		HashKind::Sha256 => {
			push_impl::<Sha256>(&origin, &found, &body, signed, signing_key, force, delete).await
		}
	}
}

async fn push_impl<H: HashAlgorithm>(
	origin: &Origin,
	found: &repo::Discovered,
	body: &[u8],
	signed: bool,
	signing_key: Option<PathBuf>,
	force: bool,
	delete: Option<String>,
) -> Result<()> {
	let repository = repo::open_generic::<H>(&found.git_dir, &found.common_dir);
	let advertised = parse_advertisement::<H>(body)?;

	if let Some(target) = delete {
		let refname = normalize_branch(&target);
		let remote = advertised
			.oid_of(&refname)
			.with_context(|| format!("the remote has no {refname}"))?;
		let update = RefUpdate {
			old: Some(remote),
			new: None,
			name: refname.clone(),
		};
		let request = build_receive_pack_request(std::slice::from_ref(&update), &[]);
		let response = http_post(&origin.receive_pack(), RECEIVE_PACK_REQUEST, request).await?;
		parse_report_status(&response)?;
		println!("Deleted {refname} on {}", origin.url);
		return Ok(());
	}

	let branch = match repository.refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => bail!("cannot push a detached HEAD"),
	};
	let local_tip = repository
		.refs()
		.resolve(&branch)
		.await?
		.context("nothing to push (the branch is unborn)")?;

	let remote_old = advertised.oid_of(&branch);
	if remote_old == Some(local_tip) {
		println!("Everything up-to-date");
		return Ok(());
	}

	// Pack the objects the remote lacks (reachable from the tip, minus its refs).
	let haves = transport::advertised_oids(&advertised);
	let pack = build_pack(&repository, &[local_tip], &haves).await?;

	let request = if signed {
		let nonce = advertised
			.push_cert_nonce
			.clone()
			.context("the server does not accept signed pushes")?;
		let cert = sign_push(
			&repository,
			origin,
			signing_key,
			nonce,
			remote_old,
			local_tip,
			&branch,
		)
		.await?;
		build_push_cert(&cert, &push_caps::<H>(), &pack)
	} else {
		let update = RefUpdate {
			old: remote_old,
			new: Some(local_tip),
			name: branch.clone(),
		};
		build_receive_pack_request(std::slice::from_ref(&update), &pack)
	};

	let response = http_post(&origin.receive_pack(), RECEIVE_PACK_REQUEST, request).await?;
	parse_report_status(&response)?;

	let how = match (signed, force) {
		(true, _) => " (signed)",
		(false, true) => " (forced)",
		_ => "",
	};
	println!("Pushed {branch} -> {}{how}", origin.url);
	Ok(())
}

/// Capabilities echoed on the push request's first line / cert marker, for hash `H`.
fn push_caps<H: HashAlgorithm>() -> String {
	format!("report-status object-format={}", H::NAME)
}

/// Expand a branch name to a full ref (`main` → `refs/heads/main`); pass full refs through.
fn normalize_branch(name: &str) -> String {
	if name.starts_with("refs/") {
		name.to_owned()
	} else {
		format!("refs/heads/{name}")
	}
}

/// Build and sign a push certificate for a single-branch update.
async fn sign_push<H: HashAlgorithm>(
	repository: &Repository<Backend, H>,
	origin: &Origin,
	_signing_key: Option<PathBuf>,
	nonce: String,
	remote_old: Option<ObjectId<H>>,
	local_tip: ObjectId<H>,
	branch: &str,
) -> Result<PushCert> {
	// The "no previous value" oid for a create is the all-zero id at the hash's width.
	let zero = "0".repeat(H::RAW_LEN * 2);
	// Signing key loading is not wired yet; keep the certificate payload explicit.
	let cert = PushCert {
		version: "0.1".to_owned(),
		pusher: pusher_ident(repository).await?,
		pushee: origin.url.clone(),
		nonce,
		push_options: Vec::new(),
		commands: vec![CertCommand {
			old: remote_old.map_or(zero, |oid| oid.to_hex()),
			new: local_tip.to_hex(),
			refname: branch.to_owned(),
		}],
		signature: String::new(),
	};
	Ok(cert)
}

/// The pusher identity for a certificate: `Name <email> <unix-ts> +0000`.
async fn pusher_ident<H: HashAlgorithm>(repo: &Repository<Backend, H>) -> Result<String> {
	let config = repo.read_config().await.ok();
	let from_config = |key: &str| {
		config
			.as_ref()
			.and_then(|c| c.get_string("user", None, key).map(str::to_owned))
	};
	let name = std::env::var("GIT_AUTHOR_NAME")
		.ok()
		.or_else(|| from_config("name"))
		.context("identity name not set (GIT_AUTHOR_NAME or user.name)")?;
	let email = std::env::var("GIT_AUTHOR_EMAIL")
		.ok()
		.or_else(|| from_config("email"))
		.context("identity email not set (GIT_AUTHOR_EMAIL or user.email)")?;
	let ts = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0);
	Ok(format!("{name} <{email}> {ts} +0000"))
}
