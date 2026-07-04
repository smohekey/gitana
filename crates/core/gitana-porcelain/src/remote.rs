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
use gitana_remote::{Origin, RECEIVE_PACK_REQUEST, Refspec, http_post};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

/// The outcome of a [`fetch`]: the tracking refs it advanced, and any it declined to.
pub struct FetchOutcome<H: HashAlgorithm> {
	/// Tracking refs advanced (`(ref name, new oid)`).
	pub updated: Vec<(String, ObjectId<H>)>,
	/// Tracking refs left unchanged because the update was not a fast-forward and the matching refspec
	/// was not forced (no leading `+`) — git rejects these too.
	pub rejected: Vec<String>,
}

/// Fetch from `origin`, downloading the objects we do not already have and updating the tracking refs
/// its configured `remote.origin.fetch` refspecs map the advertised refs to (falling back to the
/// default `+refs/heads/*:refs/remotes/origin/*` when none are configured). `advertisement` is the
/// already-fetched `GET /info/refs` body; the working tree is not touched.
///
/// Each advertised ref is matched against the positive refspecs (the first that maps it wins) unless a
/// negative `^<pattern>` refspec excludes it. A non-forced refspec advances its tracking ref only on a
/// fast-forward; a non-fast-forward under such a refspec is reported in [`FetchOutcome::rejected`].
///
/// A plain fetch refuses to write the branch HEAD points at (git's rule — the work tree is not
/// updated here). `update_head_ok` (set by `pull`) instead *skips* that destination silently: `pull`
/// advances the checked-out branch and work tree through its merge step.
pub async fn fetch<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	update_head_ok: bool,
) -> Result<FetchOutcome<H>> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let haves = gitana_remote::local_haves(repo).await?;
	download(repo, origin, &advertised, &haves).await?;

	let config = repo.read_config().await?;
	let refspecs = parse_fetch_refspecs(&config)?;
	let (positive, negative): (Vec<&Refspec>, Vec<&Refspec>) =
		refspecs.iter().partition(|spec| !spec.negative);

	// An exact (non-wildcard) source that the remote does not advertise is an error, as git reports
	// `couldn't find remote ref …` — so a typo or deleted branch in the config is not a silent success.
	for spec in &positive {
		if let Some(source) = spec.exact_source()
			&& !advertised.refs.iter().any(|(name, _)| name == source)
		{
			bail!("couldn't find remote ref {source}");
		}
	}

	// The branch HEAD points at may not be fetched into directly — git refuses, because a plain fetch
	// does not update the work tree. A bare repo has no work tree, so git allows it there (e.g. a
	// `+refs/heads/*:refs/heads/*` mirror); `pull` allows it too and reconciles the work tree via merge.
	let bare = config
		.get_bool("core", None, "bare")
		.ok()
		.flatten()
		.unwrap_or(false);
	let checked_out = match (bare, repo.refs().read_head().await?) {
		(false, HeadState::Symbolic(branch)) => Some(branch),
		_ => None,
	};

	// Plan the tracking-ref updates first: every positive refspec that maps an advertised ref writes
	// its destination (git applies them all), deduped so one source→destination pair acts once. Two
	// *different* sources mapping to the same destination is a config error git aborts on, so detect
	// it before writing anything.
	let mut plan: Vec<(ObjectId<H>, String, bool)> = Vec::new();
	let mut claimed: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
	let mut rejected = Vec::new();
	for (name, oid) in &advertised.refs {
		if negative.iter().any(|spec| spec.excludes(name)) {
			continue;
		}
		for spec in &positive {
			let Some(tracking) = spec.destination(name) else {
				continue;
			};
			// Conflict detection covers every destination, including the checked-out branch: two
			// different sources targeting one local ref is a config error git aborts on.
			match claimed.get(tracking.as_str()) {
				Some(&other) if other != name.as_str() => {
					bail!("cannot fetch both {other} and {name} to {tracking}");
				}
				Some(_) => continue, // this source already maps here (via another refspec)
				None => {
					claimed.insert(tracking.clone(), name.as_str());
				}
			}
			if checked_out.as_deref() == Some(tracking.as_str()) {
				if !update_head_ok {
					bail!("refusing to fetch into branch '{tracking}' checked out in the work tree");
				}
				// pull advances the checked-out branch via its merge step, not here — but only on a
				// fast-forward. A non-fast-forward onto the current branch is refused (git would
				// force-reset it under a `+` refspec, discarding local commits — we decline to do that
				// silently); a non-forced refspec would have git reject it too.
				let current = repo.refs().resolve(&tracking).await?;
				if current != Some(*oid)
					&& let Some(current) = current
					&& !repo.is_ancestor(current, *oid).await?
				{
					rejected.push(tracking);
				}
				continue;
			}
			plan.push((*oid, tracking, spec.force));
		}
	}

	let mut updated = Vec::new();
	for (oid, tracking, force) in plan {
		let current = repo.refs().resolve(&tracking).await?;
		if current == Some(oid) {
			continue;
		}
		// Without `+`, only a fast-forward is allowed (a fresh tracking ref always is).
		if !force
			&& let Some(current) = current
			&& !repo.is_ancestor(current, oid).await?
		{
			rejected.push(tracking);
			continue;
		}
		repo.refs().update_ref(&tracking, oid, current).await?;
		updated.push((tracking, oid));
	}
	Ok(FetchOutcome { updated, rejected })
}

/// The parsed `remote.origin.fetch` refspecs, or the default when the remote has no `fetch` line.
fn parse_fetch_refspecs(config: &gitana_config::GitConfig) -> Result<Vec<Refspec>> {
	let configured = config.get_all("remote", Some("origin"), "fetch");
	if configured.is_empty() {
		return Ok(vec![Refspec::parse(gitana_remote::ORIGIN_FETCH_REFSPEC)?]);
	}
	configured.iter().map(|spec| Refspec::parse(spec)).collect()
}

/// The upstream tip `pull` should merge for local `branch`, read from the advertisement (the merge
/// source, independent of where the fetch refspecs route tracking refs — under a mirror refspec they
/// may not write one for the branch at all).
///
/// If a positive refspec maps an advertised source directly onto `branch` (a mirror `refs/heads/*:
/// refs/heads/*`, or a rename `refs/heads/trunk:refs/heads/main`), that source's tip is the upstream —
/// so pull follows the ref the config actually fetched onto the branch, not blindly the same name.
/// Otherwise the branch is fetched into a tracking ref and the upstream is the same-named remote
/// branch. Either way a negative refspec on the chosen source yields `None`: git then declines to
/// merge ("no such ref was fetched"); a non-fast-forward rejection surfaces via
/// [`FetchOutcome::rejected`].
pub async fn pull_upstream<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	advertisement: &[u8],
	branch: &str,
) -> Result<Option<ObjectId<H>>> {
	let config = repo.read_config().await?;
	let refspecs = parse_fetch_refspecs(&config)?;
	let advertised = parse_advertisement::<H>(advertisement)?;
	let excluded = |name: &str| refspecs.iter().any(|spec| spec.excludes(name));
	let maps_onto_branch = |name: &str| {
		refspecs
			.iter()
			.any(|spec| spec.destination(name).as_deref() == Some(branch))
	};
	// A source a refspec maps onto the branch wins; else fall back to the same-named remote branch.
	if let Some((_, oid)) = advertised
		.refs
		.iter()
		.find(|(name, _)| !excluded(name) && maps_onto_branch(name))
	{
		return Ok(Some(*oid));
	}
	if excluded(branch) {
		return Ok(None);
	}
	Ok(advertised.oid_of(branch))
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
