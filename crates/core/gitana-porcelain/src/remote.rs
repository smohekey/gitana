//! Remote composites over `gitana-remote`'s Smart-HTTP primitives — `clone`, `fetch`, and `push`.
//! Each returns data; the CLI adapter fetches the ref advertisement (hash-agnostic) and dispatches the
//! hash algorithm, then calls these generic over the file store. (`pull` composes `fetch` + `merge` in
//! the adapter.)

use std::path::Path;

use anyhow::{Context, Result, bail};
use gitana_file_store::FileStore;
use gitana_git_http::{
	Advertised, CertCommand, PushCert, RefUpdate, build_pack, build_push_cert,
	build_receive_pack_request, parse_advertisement, parse_report_status,
};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_remote::{Origin, RECEIVE_PACK_REQUEST, http_post};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

/// The remote-tracking refs a [`fetch`] advanced (`(name, new oid)`).
pub struct FetchOutcome<H: HashAlgorithm> {
	pub updated: Vec<(String, ObjectId<H>)>,
}

/// Fetch every advertised branch from `origin` into `refs/remotes/origin/*`, downloading the objects
/// we do not already have. `advertisement` is the already-fetched `GET /info/refs` body; the working
/// tree is not touched.
pub async fn fetch<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
) -> Result<FetchOutcome<H>> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let haves = gitana_remote::local_haves(repo).await?;
	download(repo, origin, &advertised, &haves).await?;

	let mut updated = Vec::new();
	for (name, oid) in advertised.branches() {
		let short = name.strip_prefix("refs/heads/").unwrap_or(name);
		let tracking = format!("refs/remotes/origin/{short}");
		let current = repo.refs().resolve(&tracking).await?;
		if current != Some(oid) {
			repo.refs().update_ref(&tracking, oid, current).await?;
			updated.push((tracking, oid));
		}
	}
	Ok(FetchOutcome { updated })
}

/// Clone the advertised repository into `work_dir` (whose `.git` backs `repo`): initialise it (writing
/// a config matching `H`), download every advertised tip, recreate the refs and `HEAD`, save the
/// origin, and check out `HEAD`. `advertisement` is the already-fetched `GET /info/refs` body.
pub async fn clone<F: FileStore, H: HashAlgorithm>(
	repo: Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	work_dir: &Path,
) -> Result<()> {
	let git_dir = work_dir.join(".git");
	repo.init().await?;

	let advertised = parse_advertisement::<H>(advertisement)?;
	download(&repo, origin, &advertised, &[]).await?;

	// Recreate the refs and HEAD locally.
	for (name, oid) in &advertised.refs {
		if name.starts_with("refs/") {
			repo.refs().update_ref(name, *oid, None).await?;
		}
	}
	let head_target = advertised
		.head_target
		.clone()
		.unwrap_or_else(|| "refs/heads/main".to_owned());
	repo.refs().set_head_symbolic(&head_target).await?;
	origin.save(&git_dir)?;

	// Populate the working tree from HEAD (if the repo had any commits).
	if let Some(commit) = repo.refs().resolve_head().await? {
		let tree = repo.commit_tree(commit).await?;
		let worktree = WorkTree::new(repo, work_dir, git_dir);
		worktree.checkout(tree, true).await?;
	}
	Ok(())
}

/// Download the objects reachable from the advertised tips that `haves` do not already cover, writing
/// them into `repo`.
async fn download<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	origin: &Origin,
	advertised: &Advertised<H>,
	haves: &[ObjectId<H>],
) -> Result<()> {
	let wants = gitana_remote::advertised_oids(advertised);
	gitana_remote::fetch_pack(origin, repo, &wants, haves).await?;
	Ok(())
}

/// The result of a [`push`]; `signed`/`forced` are render hints.
pub enum PushOutcome {
	/// `HEAD`'s branch was pushed to the remote.
	Pushed {
		branch: String,
		signed: bool,
		forced: bool,
	},
	/// A remote branch was deleted.
	Deleted { refname: String },
	/// The remote already had the branch tip; nothing was sent.
	UpToDate,
}

/// Push `HEAD`'s branch to `origin` (or, with `delete`, remove a remote branch). `advertisement` is the
/// already-fetched `git-receive-pack` `GET /info/refs` body. For a signed push, `pusher` resolves the
/// certificate's pusher line — called only after confirming the server offers push-cert, so an
/// unconfigured identity does not mask "the server does not accept signed pushes".
pub async fn push<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	force: bool,
	delete: Option<String>,
	signed: bool,
	pusher: impl AsyncFnOnce() -> Result<String>,
) -> Result<PushOutcome> {
	let advertised = parse_advertisement::<H>(advertisement)?;

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
		return Ok(PushOutcome::Deleted { refname });
	}

	let branch = match repo.refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => bail!("cannot push a detached HEAD"),
	};
	let local_tip = repo
		.refs()
		.resolve(&branch)
		.await?
		.context("nothing to push (the branch is unborn)")?;

	let remote_old = advertised.oid_of(&branch);
	if remote_old == Some(local_tip) {
		return Ok(PushOutcome::UpToDate);
	}

	// Pack the objects the remote lacks (reachable from the tip, minus its refs).
	let haves = gitana_remote::advertised_oids(&advertised);
	let pack = build_pack(repo, &[local_tip], &haves).await?;

	let request = if signed {
		let nonce = advertised
			.push_cert_nonce
			.clone()
			.context("the server does not accept signed pushes")?;
		let cert = build_cert(
			origin,
			pusher().await?,
			nonce,
			remote_old,
			local_tip,
			&branch,
		);
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
	Ok(PushOutcome::Pushed {
		branch,
		signed,
		forced: force,
	})
}

/// Build a push certificate for a single-branch update. Signing itself is not yet wired, so the
/// `signature` is left empty; the payload is otherwise complete.
fn build_cert<H: HashAlgorithm>(
	origin: &Origin,
	pusher: String,
	nonce: String,
	remote_old: Option<ObjectId<H>>,
	local_tip: ObjectId<H>,
	branch: &str,
) -> PushCert {
	// The "no previous value" oid for a create is the all-zero id at the hash's width.
	let zero = "0".repeat(H::RAW_LEN * 2);
	PushCert {
		version: "0.1".to_owned(),
		pusher,
		pushee: origin.url.clone(),
		nonce,
		push_options: Vec::new(),
		commands: vec![CertCommand {
			old: remote_old.map_or(zero, |oid| oid.to_hex()),
			new: local_tip.to_hex(),
			refname: branch.to_owned(),
		}],
		signature: String::new(),
	}
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
