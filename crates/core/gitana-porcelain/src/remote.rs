//! Remote composites over `gitana-remote`'s Smart-HTTP primitives — `clone`, `fetch`, and `push`.
//! Each returns data; the CLI adapter fetches the ref advertisement (hash-agnostic) and dispatches the
//! hash algorithm, then calls these generic over the file store. (`pull` composes `fetch` + `merge` in
//! the adapter.)

use anyhow::{Context, Result, bail};
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_git_http::{
	Advertised, CertCommand, PushCert, RefUpdate, build_pack, build_push_cert,
	build_receive_pack_request, parse_advertisement, parse_report_status,
};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_remote::{HttpTransport, Origin, PushRefspec, RECEIVE_PACK_REQUEST, Refspec};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::Signer;

/// How a [`fetch`] treats the remote's tags (git's default / `--tags` / `--no-tags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TagFetch {
	/// Default: auto-follow. Beyond the configured refspecs, write `refs/tags/<name>` for each
	/// advertised tag that is not already present locally and whose target is reachable from a branch
	/// this fetch is following — matching git, which follows tags pointing into the fetched history.
	#[default]
	Auto,
	/// `--tags`: fetch every advertised `refs/tags/*` into the same-named local `refs/tags/*`.
	All,
	/// `--no-tags`: write no tag refs beyond what the configured refspecs name (disables auto-follow).
	None,
}

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
	transport: &impl HttpTransport,
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	update_head_ok: bool,
	tags: TagFetch,
) -> Result<FetchOutcome<H>> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let haves = gitana_remote::local_haves(repo).await?;
	download(transport, repo, origin, &advertised, &haves).await?;

	let config = repo.read_config().await?;
	// Resolve the effective tag mode: an explicit CLI `--tags` / `--no-tags` (`All` / `None`) wins;
	// otherwise the default (`Auto`) honors git's `remote.origin.tagOpt` config (`--tags` / `--no-tags`,
	// set e.g. by `git clone --no-tags`), and only then falls back to auto-follow.
	let tags = match tags {
		TagFetch::Auto => match config.get_string("remote", Some("origin"), "tagopt") {
			Some("--tags") => TagFetch::All,
			Some("--no-tags") => TagFetch::None,
			_ => TagFetch::Auto,
		},
		explicit => explicit,
	};
	let mut refspecs = parse_fetch_refspecs(&config)?;
	// `--tags` mirrors every advertised tag into the same-named local ref, in addition to the
	// configured refspecs. It is not forced (git does not clobber an existing tag pointing elsewhere
	// without `--force`), so a tag that would move to an unrelated object is reported as rejected.
	if tags == TagFetch::All {
		refspecs.push(Refspec::parse("refs/tags/*:refs/tags/*")?);
	}
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
		// Without `+`, a non-forced update to an existing ref is refused unless allowed. A branch may
		// advance on a fast-forward; but an existing tag is immutable — git rejects repointing a
		// `refs/tags/*` that already exists to a different object even on a fast-forward (a tag is a
		// fixed name, not a moving branch tip). A fresh tracking ref (`current` is `None`) always
		// applies. (The push side enforces the same tag immutability in `plan_push`.)
		if !force && let Some(existing) = current {
			let allowed = !tracking.starts_with("refs/tags/") && repo.is_ancestor(existing, oid).await?;
			if !allowed {
				rejected.push(tracking);
				continue;
			}
		}
		repo.refs().update_ref(&tracking, oid, current).await?;
		updated.push((tracking, oid));
	}

	// Auto-follow (git's default): create a local `refs/tags/<name>` for each advertised tag that is
	// not already present and whose target is reachable from a branch this fetch is following. `--tags`
	// mirrors tags through a refspec above (so it skips this); `--no-tags` disables it.
	if tags == TagFetch::Auto {
		auto_follow_tags(repo, &advertised, &positive, &negative, &mut updated).await?;
	}
	Ok(FetchOutcome { updated, rejected })
}

/// Write a local `refs/tags/<name>` for each advertised tag whose target is reachable from a branch
/// this fetch is following (git's tag auto-follow), skipping tags already present locally (git leaves
/// existing tags alone in auto mode — it never clobbers or reports them here). "Reachable" means the
/// tag's target — the commit, tree, or blob it ultimately points at — is in the object closure of the
/// fetched branch tips: git follows a tag pointing at any object the branch fetch downloads, including
/// a tree or blob, but not one on history this fetch did not pull.
///
/// gitana already downloads the whole advertised closure (not just the branches), so the objects are
/// present and peeling / the closure walk run locally. The walk is bounded by the branch closure and
/// only runs when there is a candidate tag; a future refspec-scoped download (protocol slice) could
/// reduce this to an O(1) presence check per tag, as git does.
async fn auto_follow_tags<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	advertised: &Advertised<H>,
	positive: &[&Refspec],
	negative: &[&Refspec],
	updated: &mut Vec<(String, ObjectId<H>)>,
) -> Result<()> {
	// Candidate tags: advertised `refs/tags/*` not already present locally (git leaves existing tags
	// alone in auto mode). If none, skip the reachability walk entirely.
	let mut candidates = Vec::new();
	for (name, oid) in &advertised.refs {
		if name.starts_with("refs/tags/") && repo.refs().resolve(name).await?.is_none() {
			candidates.push((name.clone(), *oid));
		}
	}
	if candidates.is_empty() {
		return Ok(());
	}

	// Reachability roots: the tips of the non-tag refs this fetch follows (its branch tips). A tag is
	// auto-followed only when its target lies in the object closure of these — so a tag on history this
	// fetch did not pull (e.g. an unfetched branch, or a standalone object) is left alone, as git does.
	let roots: Vec<ObjectId<H>> = advertised
		.refs
		.iter()
		.filter(|(name, _)| !name.starts_with("refs/tags/"))
		.filter(|(name, _)| !negative.iter().any(|spec| spec.excludes(name)))
		.filter(|(name, _)| positive.iter().any(|spec| spec.destination(name).is_some()))
		.map(|(_, oid)| *oid)
		.collect();
	let closure = crate::prune::reachable_from(repo, roots).await?;

	for (name, oid) in candidates {
		// Peel through any tag chain to the object the tag ultimately names (commit / tree / blob).
		let target = peel_tag_target(repo, oid).await?;
		if closure.contains(&target) {
			repo.refs().update_ref(&name, oid, None).await?;
			updated.push((name, oid));
		}
	}
	Ok(())
}

/// Follow an annotated-tag chain from `oid` to the object it ultimately points at (a commit, tree, or
/// blob). A lightweight tag's `oid` is already that object. A read error propagates: the fetch just
/// downloaded the advertised closure, so an unreadable tag object indicates a corrupt pack / store,
/// not a benign miss. (Mirrors gitana-git-http's private best-effort `peel_tag` on the client side —
/// a candidate to share, though the error handling differs by context.)
async fn peel_tag_target<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	oid: ObjectId<H>,
) -> Result<ObjectId<H>> {
	let mut current = oid;
	loop {
		let (kind, data) = repo.objects().read_object(&current).await?;
		if kind == gitana_object::ObjectKind::Tag {
			current = gitana_object::parse_tag::<H>(&data)?.object;
		} else {
			return Ok(current);
		}
	}
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

/// Clone the advertised repository into `work` (whose `.git` backs `repo`): initialise it (writing
/// a config matching `H`), download every advertised tip, recreate the refs and `HEAD`, save the
/// origin, and check out `HEAD`. `advertisement` is the already-fetched `GET /info/refs` body. The
/// origin is persisted through `repo`'s file store (no ambient filesystem access), so this runs over
/// any [`FileStore`] — a local checkout or the wasm descriptor backend.
pub async fn clone<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	transport: &impl HttpTransport,
	repo: Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	work: W,
) -> Result<()> {
	repo.init().await?;

	let advertised = parse_advertisement::<H>(advertisement)?;
	download(transport, &repo, origin, &advertised, &[]).await?;

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
	origin.save(repo.objects().file_store()).await?;

	// Populate the working tree from HEAD (if the repo had any commits).
	if let Some(commit) = repo.refs().resolve_head().await? {
		let tree = repo.commit_tree(commit).await?;
		// The `git_dir` a `WorkTree` carries is inert — the index and all git-dir files route through
		// the `FileStore` — so a placeholder path suffices, as elsewhere in the worktree layer.
		let worktree = WorkTree::new(repo, work, "");
		worktree.checkout(tree, true).await?;
	}
	Ok(())
}

/// Download the objects reachable from the advertised tips that `haves` do not already cover, writing
/// them into `repo`.
async fn download<F: FileStore, H: HashAlgorithm>(
	transport: &impl HttpTransport,
	repo: &Repository<F, H>,
	origin: &Origin,
	advertised: &Advertised<H>,
	haves: &[ObjectId<H>],
) -> Result<()> {
	let wants = gitana_remote::advertised_oids(advertised);
	gitana_remote::fetch_pack(transport, origin, repo, &wants, haves).await?;
	Ok(())
}

/// The result of a [`push`]. `results` is empty when everything was already up to date; `signed` is a
/// render hint.
pub struct PushOutcome {
	/// One entry per destination ref actually updated or deleted.
	pub results: Vec<PushResult>,
	/// Whether the push was sent as a signed certificate.
	pub signed: bool,
}

/// One destination ref's outcome within a [`PushOutcome`].
pub struct PushResult {
	/// The remote ref updated or deleted.
	pub refname: String,
	/// Whether this was a deletion.
	pub deleted: bool,
	/// Whether the update was forced (a non-fast-forward permitted).
	pub forced: bool,
}

impl PushOutcome {
	/// Whether nothing was sent because every ref was already up to date.
	pub fn is_up_to_date(&self) -> bool {
		self.results.is_empty()
	}
}

/// A planned ref update (already fast-forward-checked and resolved) plus its render metadata.
struct Planned<H: HashAlgorithm> {
	update: RefUpdate<H>,
	forced: bool,
}

/// Which tags a [`push`] sends beyond its refspecs (git's `--tags` / `--follow-tags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PushTags {
	/// No tags beyond those the refspecs name explicitly.
	#[default]
	None,
	/// `--tags`: push every local `refs/tags/*`. With no explicit refspec this pushes tags *only* —
	/// git suppresses the default branch push when `--tags` supplies the refs.
	All,
	/// `--follow-tags`: in addition to the base push, send annotated tags reachable from the pushed
	/// commits that the remote does not already have.
	Follow,
}

/// Push `refspecs` to `origin` over an unsigned receive-pack request. Each `[+]<src>:<dst>` maps a
/// local ref to a remote ref (an empty `<src>` deletes it); an empty `refspecs` list pushes what
/// `remote.origin.push` configures, else `HEAD`'s branch to the same-name remote branch. `tags` adds
/// tags per git's `--tags` / `--follow-tags`. `advertisement` is the already-fetched
/// `git-receive-pack` `GET /info/refs` body. For a signed push (`gta push --signed`), see
/// [`push_signed`].
pub async fn push<F: FileStore, H: HashAlgorithm>(
	transport: &impl HttpTransport,
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	force: bool,
	refspecs: Vec<PushRefspec>,
	tags: PushTags,
) -> Result<PushOutcome> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let planned = plan_push(repo, &advertised, refspecs, force, tags).await?;
	if planned.is_empty() {
		return Ok(PushOutcome {
			results: Vec::new(),
			signed: false,
		});
	}
	let updates: Vec<RefUpdate<H>> = planned.iter().map(|p| p.update.clone()).collect();
	let pack = pack_for(repo, &advertised, &planned).await?;
	let request = build_receive_pack_request(&updates, &pack);
	send_receive_pack(transport, origin, request).await?;
	Ok(PushOutcome {
		results: results_of(&planned),
		signed: false,
	})
}

/// Push `refspecs` to `origin` with a signed push certificate (`gta push --signed`) — a single cert
/// carrying one command per ref, so a `require` server can authorise every update (and any deletion).
/// Otherwise like [`push`]. `pusher` resolves the certificate's pusher line and `signer` signs the
/// certificate body — both invoked only after confirming the server offers push-cert, so an
/// unconfigured identity or an unresolvable signing key does not mask "the server does not accept
/// signed pushes".
pub async fn push_signed<F: FileStore, H: HashAlgorithm, S: Signer>(
	transport: &impl HttpTransport,
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	force: bool,
	refspecs: Vec<PushRefspec>,
	tags: PushTags,
	pusher: impl AsyncFnOnce() -> Result<String>,
	signer: &S,
) -> Result<PushOutcome> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let planned = plan_push(repo, &advertised, refspecs, force, tags).await?;
	if planned.is_empty() {
		return Ok(PushOutcome {
			results: Vec::new(),
			signed: true,
		});
	}
	let nonce = advertised
		.push_cert_nonce
		.clone()
		.context("the server does not accept signed pushes")?;
	let updates: Vec<RefUpdate<H>> = planned.iter().map(|p| p.update.clone()).collect();
	let mut cert = build_cert(origin, pusher().await?, nonce, &updates);
	// The signer emits an SSHSIG armor (git's `git` namespace) over the certificate body — exactly what
	// receive-pack verifies via `verify_sshsig(cert.payload(), cert.signature, keys, "git")`.
	cert.signature = signer.sign(&cert.payload()).await?;
	let pack = pack_for(repo, &advertised, &planned).await?;
	let request = build_push_cert(&cert, &push_caps::<H>(), &pack);
	send_receive_pack(transport, origin, request).await?;
	Ok(PushOutcome {
		results: results_of(&planned),
		signed: true,
	})
}

/// Resolve `refspecs` to concrete, fast-forward-checked ref updates against the advertised refs. An
/// empty `refspecs` defaults to `remote.origin.push`, else `HEAD`'s branch pushed to the same name. A
/// ref already at its destination tip is dropped (nothing to send for it). A non-fast-forward update
/// without `force` (global) or a `+` on the refspec is refused before anything is sent.
async fn plan_push<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	advertised: &Advertised<H>,
	refspecs: Vec<PushRefspec>,
	force: bool,
	tags: PushTags,
) -> Result<Vec<Planned<H>>> {
	// The base refspecs: explicit ones, else the config/HEAD default — except `--tags` with no explicit
	// refspec pushes tags *only* (git suppresses the branch default when `--tags` supplies the refs).
	let base = if refspecs.is_empty() && tags == PushTags::All {
		Vec::new()
	} else {
		default_refspecs(repo, refspecs).await?
	};
	// Expand `--tags` / `--follow-tags` into additional `refs/tags/*` refspecs (see `tag_refspecs`).
	let tag_specs = tag_refspecs(repo, advertised, &base, tags).await?;
	let refspecs: Vec<PushRefspec> = base.into_iter().chain(tag_specs).collect();
	let mut planned = Vec::new();
	let mut seen_dsts = std::collections::HashSet::new();
	for spec in refspecs {
		let forced = force || spec.force;
		// A `HEAD` destination (git's `push origin HEAD` shorthand) means the current branch's ref.
		let dst = resolve_destination(repo, &spec.dst).await?;
		// Two refspecs targeting one destination would be applied sequentially by receive-pack — the
		// second stale after the first moved the ref — leaving a partial push. Refuse before sending.
		if !seen_dsts.insert(dst.clone()) {
			bail!("refspec destination {dst} is updated by more than one refspec");
		}
		let remote_old = advertised.oid_of(&dst);
		let update = match &spec.src {
			// Deletion: the remote ref must exist.
			None => {
				let old = remote_old.with_context(|| format!("the remote has no {dst}"))?;
				RefUpdate {
					old: Some(old),
					new: None,
					name: dst,
				}
			}
			Some(src) => {
				let local_tip = resolve_source(repo, src)
					.await?
					.with_context(|| format!("{src} does not resolve to a ref to push"))?;
				if remote_old == Some(local_tip) {
					continue; // already up to date
				}
				// Existing tags are immutable in git: any change to an existing `refs/tags/*` needs a
				// force, even a fast-forward — a tag is a fixed name, not a moving branch tip. (Bare-name
				// tag DWIM — `push origin v1` where v1 is a local tag — is deferred to the tags slice.)
				if !forced && dst.starts_with("refs/tags/") && remote_old.is_some() {
					bail!(
						"updates to {dst} were rejected: the tag already exists on the remote at a different \
							 object; force with `+{src}:{dst}` / --force to overwrite it"
					);
				}
				// Otherwise, without a force, refuse a non-fast-forward before sending anything (git's client-side
				// check): the remote tip must be an ancestor of the local tip. A create is a fast-forward.
				if !forced
					&& let Some(old) = remote_old
					&& !repo.is_ancestor(old, local_tip).await?
				{
					bail!(
						"updates to {dst} were rejected: the remote has work you do not have locally; fetch \
						 and integrate first, or force with `+{src}:{dst}` / --force"
					);
				}
				RefUpdate {
					old: remote_old,
					new: Some(local_tip),
					name: dst,
				}
			}
		};
		planned.push(Planned { update, forced });
	}
	Ok(planned)
}

/// The extra `refs/tags/<name>:refs/tags/<name>` refspecs `--tags` / `--follow-tags` add to the base
/// push. `--tags` sends every local tag; `--follow-tags` sends only annotated tags whose target commit
/// is reachable from the commits being pushed (`base`) and that the remote does not already have. A tag
/// whose destination a base refspec already targets is skipped, so it is planned once. The planner then
/// applies the usual create / immutability / fast-forward rules to each.
async fn tag_refspecs<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	advertised: &Advertised<H>,
	base: &[PushRefspec],
	tags: PushTags,
) -> Result<Vec<PushRefspec>> {
	if tags == PushTags::None {
		return Ok(Vec::new());
	}
	// Destinations the base already pushes, so a tag is not planned twice (git de-dups these).
	let mut base_dsts = std::collections::HashSet::new();
	for spec in base {
		base_dsts.insert(resolve_destination(repo, &spec.dst).await?);
	}
	// For `--follow-tags`: the object closure of the commits being pushed (the base sources' tips),
	// against which each candidate annotated tag's target commit is tested for reachability.
	let reachable = if tags == PushTags::Follow {
		let mut tips = Vec::new();
		for spec in base {
			if let Some(src) = &spec.src
				&& let Some(tip) = resolve_source(repo, src).await?
			{
				tips.push(tip);
			}
		}
		Some(crate::prune::reachable_from(repo, tips).await?)
	} else {
		None
	};

	let mut out = Vec::new();
	for (name, oid) in repo.refs().list("refs/tags/").await? {
		if base_dsts.contains(&name) {
			continue;
		}
		let include = match tags {
			PushTags::All => true,
			// A follow candidate is an annotated tag, missing from the remote, whose target commit is
			// reachable from a pushed commit. Lightweight tags and tags of non-commits are not followed.
			PushTags::Follow => {
				advertised.oid_of(&name).is_none()
					&& match follow_tag_commit(repo, oid).await? {
						Some(commit) => reachable.as_ref().is_some_and(|set| set.contains(&commit)),
						None => false,
					}
			}
			PushTags::None => unreachable!("returned early above"),
		};
		if include {
			out.push(PushRefspec {
				force: false,
				src: Some(name.clone()),
				dst: name,
			});
		}
	}
	Ok(out)
}

/// The commit an annotated tag ultimately points at (peeling nested tags), or `None` if `oid` is a
/// lightweight tag (not a tag object) or the tag does not resolve to a commit — neither is a
/// `--follow-tags` candidate. A read error propagates (the tag is a local ref we expect to resolve).
async fn follow_tag_commit<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	oid: ObjectId<H>,
) -> Result<Option<ObjectId<H>>> {
	let (kind, data) = repo.objects().read_object(&oid).await?;
	if kind != gitana_object::ObjectKind::Tag {
		return Ok(None);
	}
	let mut target = gitana_object::parse_tag::<H>(&data)?.object;
	loop {
		let (kind, data) = repo.objects().read_object(&target).await?;
		match kind {
			gitana_object::ObjectKind::Commit => return Ok(Some(target)),
			gitana_object::ObjectKind::Tag => target = gitana_object::parse_tag::<H>(&data)?.object,
			_ => return Ok(None),
		}
	}
}

/// Resolve a push destination: the literal `HEAD` becomes the current branch's ref; any other name is
/// already a full ref.
async fn resolve_destination<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	dst: &str,
) -> Result<String> {
	if dst == "HEAD" {
		return match repo.refs().read_head().await? {
			HeadState::Symbolic(branch) => Ok(branch),
			HeadState::Detached(_) => {
				bail!("cannot push to `HEAD` from a detached HEAD; use an explicit destination ref")
			}
		};
	}
	Ok(dst.to_owned())
}

/// The refspecs to push: `refspecs` when non-empty, else `remote.origin.push`, else `HEAD`'s branch to
/// the same-name remote branch (git's default when nothing is configured).
async fn default_refspecs<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	refspecs: Vec<PushRefspec>,
) -> Result<Vec<PushRefspec>> {
	if !refspecs.is_empty() {
		return Ok(refspecs);
	}
	if let Ok(config) = repo.read_config().await {
		let configured = config.get_all("remote", Some("origin"), "push");
		if !configured.is_empty() {
			return configured.iter().map(|s| PushRefspec::parse(s)).collect();
		}
	}
	let branch = match repo.refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => {
			bail!("cannot push a detached HEAD without a refspec (e.g. `HEAD:refs/heads/<name>`)")
		}
	};
	Ok(vec![PushRefspec {
		force: false,
		src: Some(branch.clone()),
		dst: branch,
	}])
}

/// Resolve a push source ref to a tip: `HEAD` follows the symbolic ref (or is the detached commit); any
/// other name resolves directly.
async fn resolve_source<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	src: &str,
) -> Result<Option<ObjectId<H>>> {
	if src == "HEAD" {
		return Ok(match repo.refs().read_head().await? {
			HeadState::Symbolic(branch) => repo.refs().resolve(&branch).await?,
			HeadState::Detached(oid) => Some(oid),
		});
	}
	Ok(repo.refs().resolve(src).await?)
}

/// Build the pack the remote lacks for the planned updates (objects reachable from the new tips, minus
/// the advertised refs). A pure-deletion push sends no pack.
async fn pack_for<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	advertised: &Advertised<H>,
	planned: &[Planned<H>],
) -> Result<Vec<u8>> {
	let wants: Vec<ObjectId<H>> = planned.iter().filter_map(|p| p.update.new).collect();
	if wants.is_empty() {
		return Ok(Vec::new());
	}
	let haves = gitana_remote::advertised_oids(advertised);
	Ok(build_pack(repo, &wants, &haves).await?)
}

/// Render metadata for each planned update.
fn results_of<H: HashAlgorithm>(planned: &[Planned<H>]) -> Vec<PushResult> {
	planned
		.iter()
		.map(|p| PushResult {
			refname: p.update.name.clone(),
			deleted: p.update.new.is_none(),
			forced: p.forced,
		})
		.collect()
}

/// POST a receive-pack request to `origin` and check the report-status it returns.
async fn send_receive_pack(
	transport: &impl HttpTransport,
	origin: &Origin,
	request: Vec<u8>,
) -> Result<()> {
	let response = transport
		.post(&origin.receive_pack(), RECEIVE_PACK_REQUEST, request)
		.await?;
	parse_report_status(&response)?;
	Ok(())
}

/// Build a push certificate carrying one command per `update`, with an empty `signature`: the caller
/// signs [`PushCert::payload`] and fills it in. Each command's `old`/`new` are the ref's before/after
/// values — a `None` becomes the all-zero id, so a create is `old: None` and a delete is `new: None`.
fn build_cert<H: HashAlgorithm>(
	origin: &Origin,
	pusher: String,
	nonce: String,
	updates: &[RefUpdate<H>],
) -> PushCert {
	let zero = "0".repeat(H::RAW_LEN * 2);
	let hex = |id: Option<ObjectId<H>>| id.map_or_else(|| zero.clone(), |oid| oid.to_hex());
	PushCert {
		version: "0.1".to_owned(),
		pusher,
		pushee: origin.url.clone(),
		nonce,
		push_options: Vec::new(),
		commands: updates
			.iter()
			.map(|u| CertCommand {
				old: hex(u.old),
				new: hex(u.new),
				refname: u.name.clone(),
			})
			.collect(),
		signature: String::new(),
	}
}

/// Capabilities echoed on the push request's first line / cert marker, for hash `H`.
fn push_caps<H: HashAlgorithm>() -> String {
	format!("report-status object-format={}", H::NAME)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use std::cell::RefCell;

	use gitana_git_http::{ProtocolVersion, Service, advertise, make_nonce, peek_push_cert};
	use gitana_object::{write_flush, write_pkt};
	use gitana_trust::{TrustedKey, verify_sshsig};
	use gitana_worktree::Index;

	use super::*;
	use crate::test_support::{TestIdentity, TestSigner, fixture, stage};

	/// A [`HttpTransport`] double that records the single POSTed request and answers with a success
	/// `report-status` (`unpack ok`), so a push completes without a real server.
	struct CapturingTransport {
		posted: RefCell<Option<Vec<u8>>>,
	}

	impl HttpTransport for CapturingTransport {
		async fn get(&self, _url: &str) -> Result<Vec<u8>> {
			unreachable!("push does not GET the advertisement (the caller passes it in)")
		}

		async fn post(&self, _url: &str, _content_type: &str, body: Vec<u8>) -> Result<Vec<u8>> {
			*self.posted.borrow_mut() = Some(body);
			let mut report = Vec::new();
			write_pkt(&mut report, b"unpack ok\n").unwrap();
			write_flush(&mut report);
			Ok(report)
		}
	}

	/// A signed push attaches a certificate whose signature the trust core accepts — the same check
	/// receive-pack runs — over the exact payload, carrying the server's nonce, the pushee URL, and the
	/// create command. This is the client half of `gta push --signed`.
	#[tokio::test]
	async fn push_signed_attaches_a_verifiable_certificate() {
		// A client repo with one commit on `refs/heads/main`.
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"hello\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		wt.save_index(&index).await.unwrap();
		let tip = crate::commit(&wt, "root", &TestIdentity::default())
			.await
			.unwrap();

		// A server advertisement that offers push-cert (a nonce) and has no `refs/heads/main` yet, so
		// the push is a create — always a fast-forward, no objects to reconcile.
		let (_server_dir, server) = fixture().await;
		let nonce = make_nonce(b"secret", "acme/app", 1_700_000_000, b"\x01\x02\x03\x04");
		let advertisement = advertise(
			server.repository(),
			Service::ReceivePack,
			ProtocolVersion::V0,
			Some(&nonce),
		)
		.await
		.unwrap();

		let origin = Origin::parse("http://host/acme/app").unwrap();
		let signer = TestSigner::new(7);
		let public_line = signer.public_line();
		let transport = CapturingTransport {
			posted: RefCell::new(None),
		};

		let outcome = push_signed(
			&transport,
			wt.repository(),
			&origin,
			&advertisement,
			false,
			vec![],
			PushTags::None,
			async || Ok("Dev <dev@x.test> 1700000000 +0000".to_owned()),
			&signer,
		)
		.await
		.unwrap();
		assert!(outcome.signed && outcome.results.len() == 1 && !outcome.results[0].deleted);

		// The posted request is a push certificate binding this create to the server's nonce.
		let request = transport.posted.into_inner().expect("a request was POSTed");
		let cert = peek_push_cert(&request).expect("a signed push-cert request");
		assert_eq!(cert.nonce, nonce);
		assert_eq!(cert.pushee, origin.url);
		assert_eq!(cert.commands.len(), 1);
		assert_eq!(cert.commands[0].refname, "refs/heads/main");
		assert_eq!(cert.commands[0].new, tip.to_hex());
		assert_eq!(cert.commands[0].old, "0".repeat(64));

		// Its signature verifies over the exact payload in git's `git` namespace, under the signing key
		// — so a real server (receive-pack) that trusts this key accepts the push.
		let key = TrustedKey::from_openssh(&public_line).unwrap();
		let signer_id = verify_sshsig(
			&cert.payload(),
			cert.signature.as_bytes(),
			std::slice::from_ref(&key),
			"git",
		)
		.unwrap();
		assert_eq!(signer_id, key.id());
	}

	/// A signed delete attaches a certificate whose single command zeroes the ref's new value and whose
	/// signature the trust core accepts — so a `require` server can authorise the deletion. This is the
	/// client half of `gta push --signed --delete`.
	#[tokio::test]
	async fn push_signed_delete_attaches_a_verifiable_certificate() {
		// A server that offers push-cert and has `refs/heads/main` to delete.
		let (_server_dir, server) = fixture().await;
		let blob = server.repository().write_blob(b"srv\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		server.save_index(&index).await.unwrap();
		let server_tip = crate::commit(&server, "srv", &TestIdentity::default())
			.await
			.unwrap();
		let nonce = make_nonce(b"secret", "acme/app", 1_700_000_000, b"\x01\x02\x03\x04");
		let advertisement = advertise(
			server.repository(),
			Service::ReceivePack,
			ProtocolVersion::V0,
			Some(&nonce),
		)
		.await
		.unwrap();

		// The client repo is irrelevant to a delete (no objects are sent); any repo satisfies the type.
		let (_dir, wt) = fixture().await;
		let origin = Origin::parse("http://host/acme/app").unwrap();
		let signer = TestSigner::new(7);
		let public_line = signer.public_line();
		let transport = CapturingTransport {
			posted: RefCell::new(None),
		};

		let outcome = push_signed(
			&transport,
			wt.repository(),
			&origin,
			&advertisement,
			false,
			vec![PushRefspec::parse(":main").unwrap()],
			PushTags::None,
			async || Ok("Dev <dev@x.test> 1700000000 +0000".to_owned()),
			&signer,
		)
		.await
		.unwrap();
		assert_eq!(outcome.results.len(), 1);
		assert!(outcome.results[0].deleted);
		assert_eq!(outcome.results[0].refname, "refs/heads/main");

		// The posted request is a push certificate whose command deletes the ref (new = zero, old = tip).
		let request = transport.posted.into_inner().expect("a request was POSTed");
		let cert = peek_push_cert(&request).expect("a signed push-cert request");
		assert_eq!(cert.nonce, nonce);
		assert_eq!(cert.commands.len(), 1);
		assert_eq!(cert.commands[0].refname, "refs/heads/main");
		assert_eq!(cert.commands[0].old, server_tip.to_hex());
		assert_eq!(cert.commands[0].new, "0".repeat(64));

		// Its signature verifies over the exact payload in git's `git` namespace under the signing key —
		// so a receive-pack server that trusts this key authorises the deletion.
		let key = TrustedKey::from_openssh(&public_line).unwrap();
		verify_sshsig(
			&cert.payload(),
			cert.signature.as_bytes(),
			std::slice::from_ref(&key),
			"git",
		)
		.unwrap();
	}

	/// When the server does not advertise a nonce (push-cert unsupported), a signed push fails before
	/// resolving the pusher or touching the signer — so the error names the real cause, not a missing
	/// identity or key.
	#[tokio::test]
	async fn push_signed_without_server_support_fails_before_signing() {
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"hi\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		wt.save_index(&index).await.unwrap();
		crate::commit(&wt, "root", &TestIdentity::default())
			.await
			.unwrap();

		let (_server_dir, server) = fixture().await;
		let advertisement = advertise(
			server.repository(),
			Service::ReceivePack,
			ProtocolVersion::V0,
			None,
		)
		.await
		.unwrap();

		let origin = Origin::parse("http://host/acme/app").unwrap();
		let transport = CapturingTransport {
			posted: RefCell::new(None),
		};
		let result = push_signed(
			&transport,
			wt.repository(),
			&origin,
			&advertisement,
			false,
			vec![],
			PushTags::None,
			async || panic!("pusher resolved despite no push-cert support"),
			&crate::test_support::FailingSigner,
		)
		.await;
		let Err(err) = result else {
			panic!("a signed push to a server without push-cert support must fail");
		};
		assert!(
			err.to_string().contains("does not accept signed pushes"),
			"{err}"
		);
		assert!(
			transport.posted.into_inner().is_none(),
			"nothing was POSTed"
		);
	}

	#[tokio::test]
	async fn push_rejects_two_refspecs_targeting_one_destination() {
		// `push a:x b:x` would be applied sequentially by receive-pack: the second command is stale
		// after the first moved `x`, leaving a partial push. The plan must refuse before POSTing.
		let (_dir, wt) = fixture().await;
		let blob = wt.repository().write_blob(b"hi\n").await.unwrap();
		let mut index = Index::new();
		stage(&mut index, "f.txt", blob);
		wt.save_index(&index).await.unwrap();
		crate::commit(&wt, "root", &TestIdentity::default())
			.await
			.unwrap();
		let tip = wt
			.repository()
			.refs()
			.resolve_head()
			.await
			.unwrap()
			.unwrap();
		wt.repository()
			.refs()
			.update_ref("refs/heads/dev", tip, None)
			.await
			.unwrap();

		let (_server_dir, server) = fixture().await;
		let advertisement = advertise(
			server.repository(),
			Service::ReceivePack,
			ProtocolVersion::V0,
			None,
		)
		.await
		.unwrap();

		let origin = Origin::parse("http://host/acme/app").unwrap();
		let transport = CapturingTransport {
			posted: RefCell::new(None),
		};
		let result = push(
			&transport,
			wt.repository(),
			&origin,
			&advertisement,
			false,
			vec![
				PushRefspec::parse("main:dup").unwrap(),
				PushRefspec::parse("dev:dup").unwrap(),
			],
			PushTags::None,
		)
		.await;
		let Err(err) = result else {
			panic!("two refspecs to one destination must be rejected");
		};
		assert!(err.to_string().contains("more than one refspec"), "{err}");
		assert!(
			transport.posted.into_inner().is_none(),
			"nothing was POSTed"
		);
	}
}
