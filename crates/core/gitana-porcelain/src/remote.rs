//! Remote composites over `gitana-remote`'s Smart-HTTP primitives — `clone`, `fetch`, and `push`.
//! Each returns data; the CLI adapter fetches the ref advertisement (hash-agnostic) and dispatches the
//! hash algorithm, then calls these generic over the file store. (`pull` composes `fetch` + `merge` in
//! the adapter.)

use anyhow::{Context, Result, bail};
use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_git_http::{
	Advertised, CertCommand, Deepen, PushCert, RefUpdate, build_pack_thin, build_push_cert,
	build_receive_pack_request, parse_advertisement, parse_report_status,
};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_remote::{Connection, PackFetcher, PushRefspec, Refspec};
use gitana_repository::{HeadState, ReflogIntent, Repository};
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
	/// Tracking refs deleted because the fetch was a prune (`prune` was set) and the remote no longer
	/// advertises any ref mapping onto them — their upstream branch was deleted. Empty when `prune` is
	/// false. Judging reachability after a prune is what makes a stale tracking ref safe to trust.
	pub pruned: Vec<String>,
}

/// The reflog identity for a [`fetch`]'s tracking-ref updates: the committer line and the action
/// prefix. git writes `<action>: <status>` (e.g. `fetch origin: fast-forward`), where `<action>` is
/// its `GIT_REFLOG_ACTION` — the invoking command (`fetch origin`, or `pull origin` when a pull drives
/// the fetch). `None` (the in-component fetch, which resolves no configured identity) writes no reflog,
/// leaving the tracking refs unlogged.
pub struct FetchReflog<'a> {
	/// The reflog committer line (`Name <email> seconds ±hhmm`).
	pub committer: &'a str,
	/// The action prefix (`fetch <remote>` / `pull <remote>`).
	pub action: &'a str,
}

/// The reflog identity for a [`clone`]'s `clone: from <url>` entries (on `HEAD` and the checked-out
/// branch): the committer line and the clone source *as given*. git records the command-line URL
/// verbatim — before normalization (e.g. a trailing slash is kept) — so this carries the raw text, not
/// the parsed `Origin`. `None` (the in-component clone, with no configured identity) writes no reflog.
pub struct CloneReflog<'a> {
	/// The reflog committer line (`Name <email> seconds ±hhmm`).
	pub committer: &'a str,
	/// The clone source URL exactly as given on the command line.
	pub url: &'a str,
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
/// `checkouts` lists every branch checked out in a worktree of this repository — the current one and
/// each linked one — paired with that worktree's path (the caller enumerates them; this crate has no
/// view of the on-disk worktree layout). A refspec mapping onto a checked-out branch is refused, git's
/// way: `refusing to fetch into branch '<ref>' checked out at '<path>'`, because a plain fetch does not
/// update the work tree. The sole exception is the *current* worktree's own branch under
/// `update_head_ok` (set by `pull`): that destination is skipped here and `pull` advances the branch and
/// work tree through its merge step. Any *other* worktree's branch is refused even under
/// `update_head_ok`, since a merge advances only the current HEAD, never another worktree's branch.
///
/// A non-empty `deepen` requests a shallow update (git's `fetch --depth` / `--deepen` / `--unshallow` /
/// `--shallow-since` / `--shallow-exclude`): only the branch tips are deepened and the server's new
/// shallow boundary is folded into `.git/shallow`. An empty `deepen` (the default) is a normal fetch.
///
/// `prune` (git's `fetch --prune`) additionally deletes every tracking ref under a wildcard refspec's
/// destination namespace that no advertised ref maps to — the upstream branch was removed. This matters
/// for any caller that judges reachability from `refs/remotes/*`: without it a stale tracking ref left
/// by a deleted upstream branch keeps asserting that branch's commits are present, so the prune must run
/// before such a judgement.
///
/// `reflog` supplies the committer and action prefix for the tracking-ref reflog each advanced ref
/// records (git's `<action>: <status>`); `None` writes no reflog (see [`FetchReflog`]).
#[allow(clippy::too_many_arguments)]
pub async fn fetch<F: FileStore, H: HashAlgorithm>(
	fetcher: &mut impl PackFetcher,
	repo: &Repository<F, H>,
	advertisement: &[u8],
	update_head_ok: bool,
	tags: TagFetch,
	prune: bool,
	deepen: &Deepen,
	checkouts: &[(String, String)],
	reflog: Option<FetchReflog<'_>>,
) -> Result<FetchOutcome<H>> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let haves = gitana_remote::local_haves(repo).await?;

	// `remote.origin.*` follows git's merged precedence, so a globally-configured `tagOpt`/refspec is
	// honoured (the frontend installs the effective config; a bare engine falls back to local).
	let config = repo.effective_config().await?;
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

	// Which branch *this* worktree has checked out — the one `pull` may advance through its merge, and
	// the one destination the refusal below exempts under `update_head_ok`. A bare repo has no work tree
	// (git allows a `+refs/heads/*:refs/heads/*` mirror there), so it names no current branch; `checkouts`
	// is likewise empty for a bare repo, so nothing is refused. `core.bare` is repo identity — read it
	// from the **local** config only, never the merged view, so a global/system `core.bare` (a footgun)
	// cannot flip a non-bare worktree into refusing a fetch into its own branch (or vice versa).
	let bare = repo
		.read_config()
		.await
		.ok()
		.and_then(|local| local.get_bool("core", None, "bare").ok().flatten())
		.unwrap_or(false);
	let checked_out = match (bare, repo.refs().read_head().await?) {
		(false, HeadState::Symbolic(branch)) => Some(branch),
		_ => None,
	};

	// Validate the fetch's fatal, structural errors BEFORE downloading. A shallow fetch persists a
	// `.git/shallow` boundary inside `download`, so a fetch that then bails must not leave the repo
	// truncated (or objects half-fetched) for a command that failed. None of these checks need the
	// fetched objects — the plan loop below re-derives the tracking updates once the pack is in hand.
	validate_fetch_selection(
		&advertised,
		&positive,
		&negative,
		checked_out.as_deref(),
		update_head_ok,
		checkouts,
	)?;

	// A shallow fetch deepens from exactly the refs its refspecs select — a positive refspec's *source*
	// matches (so a source-only `refs/heads/main`, which updates no tracking ref, is still deepened) and
	// no negative refspec excludes it — matching git: each is its own shallow root. So an
	// explicitly-requested tag (via `--tags` or a tag refspec) outside branch history is fetched and
	// recorded, while a ref a negative/narrowed refspec excludes is neither fetched nor marked shallow. A
	// full fetch wants every advertised ref anyway (see `download`), so these roots are unused then.
	let deepen_roots: Vec<ObjectId<H>> = advertised
		.refs
		.iter()
		.filter(|(name, _)| !negative.iter().any(|spec| spec.excludes(name)))
		.filter(|(name, _)| positive.iter().any(|spec| spec.matches_source(name)))
		.map(|(_, oid)| *oid)
		.collect();
	// A shallow fetch asks for reachable annotated tags (`include-tag`) so tags pointing into the fetched
	// history arrive — unless `--no-tags` disabled tag fetching entirely, which git also omits it for.
	let include_tag = !deepen.is_empty() && tags != TagFetch::None;
	download(
		fetcher,
		repo,
		&advertised,
		&haves,
		deepen,
		&deepen_roots,
		include_tag,
	)
	.await?;

	// Plan the tracking-ref updates first: every positive refspec that maps an advertised ref writes
	// its destination (git applies them all), deduped so one source→destination pair acts once. Two
	// *different* sources mapping to the same destination is a config error git aborts on, so detect
	// it before writing anything.
	// Each entry is `(new oid, destination tracking ref, advertised source ref, forced)`. The source ref
	// is kept so the reflog can word a create by the *source* namespace (git's `storing head` vs
	// `storing tag`), matching git.
	let mut plan: Vec<(ObjectId<H>, String, String, bool)> = Vec::new();
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
			plan.push((*oid, tracking, name.clone(), spec.force));
		}
	}

	let mut updated = Vec::new();
	for (oid, tracking, source, force) in plan {
		let current = repo.refs().resolve(&tracking).await?;
		if current == Some(oid) {
			continue;
		}
		// Fast-forward classification is commit-only and follows annotated-tag chains, matching git: an
		// update between two commits is a fast-forward when the old is an ancestor of the new, else a
		// forced rewrite. A side that does not peel to a commit — a blob/tree tag — is neither, and git
		// records such a move as a "storing" entry. Peel *gently* (never erroring on a non-commit) so a
		// moved blob/tree tag does not abort the fetch. Skipped when nothing needs the result: a forced
		// refspec with no reflog, or a create (`current` is `None`), which is classified below anyway.
		let classify = current.is_some() && (reflog.is_some() || !force);
		let old_commit = match current {
			Some(existing) if classify => repo.try_peel_to_commit(existing).await?,
			_ => None,
		};
		let new_commit = if classify {
			repo.try_peel_to_commit(oid).await?
		} else {
			None
		};
		// A fast-forward advances one commit to a descendant — only meaningful when both sides peel to
		// commits. A side that does not (a blob/tree tag) is not a commit update at all.
		let both_commits = old_commit.is_some() && new_commit.is_some();
		let fast_forward = match (old_commit, new_commit) {
			(Some(old), Some(new)) => repo.is_ancestor(old, new).await?,
			_ => false,
		};
		// Gate a non-forced update to an existing ref (git's client-side check). A `refs/tags/*`
		// *destination* is immutable — any change needs `+` (git's "would clobber existing tag"). Elsewhere
		// a commit update must fast-forward, but a non-commit update (a blob/tree tag) git simply stores.
		// A create (`current` is `None`) always applies. (The push side enforces tag immutability in
		// `plan_push`.)
		if !force && current.is_some() {
			let allowed = !tracking.starts_with("refs/tags/") && (fast_forward || !both_commits);
			if !allowed {
				rejected.push(tracking);
				continue;
			}
		}
		// git words each advanced tracking ref as `<action>: <status>`. A *create* (or a non-commit "tag"
		// store to a non-`refs/tags/*` destination) is `storing head` / `storing tag` / `storing ref` — by
		// the *source* ref's namespace (git's `ref->name`). An update to an existing `refs/tags/*`
		// *destination* (only reachable when forced) is `updating tag`, whatever the object kind or
		// ancestry. Otherwise a commit fast-forward is `fast-forward` and a forced commit rewrite is
		// `forced-update`. `update_ref`'s own gating still skips an unlogged destination (e.g. a
		// `refs/tags/*` dest under the default `core.logAllRefUpdates`), so `--tags` stays unlogged even
		// though a message is computed here.
		let message = reflog.as_ref().map(|r| {
			let storing = if source.starts_with("refs/tags/") {
				"storing tag"
			} else if source.starts_with("refs/heads/") {
				"storing head"
			} else {
				"storing ref"
			};
			let status = if current.is_none() {
				storing
			} else if tracking.starts_with("refs/tags/") {
				"updating tag"
			} else if !both_commits {
				storing
			} else if fast_forward {
				"fast-forward"
			} else {
				"forced-update"
			};
			format!("{}: {status}", r.action)
		});
		let intent = match (&reflog, &message) {
			(Some(r), Some(msg)) => ReflogIntent::Log {
				committer: r.committer,
				message: msg,
			},
			_ => ReflogIntent::Skip,
		};
		repo
			.refs()
			.update_ref(&tracking, oid, current, intent)
			.await?;
		updated.push((tracking, oid));
	}

	// Auto-follow (git's default): create a local `refs/tags/<name>` for each advertised tag that is
	// not already present and whose target is reachable from a branch this fetch is following. `--tags`
	// mirrors tags through a refspec above (so it skips this); `--no-tags` disables it.
	if tags == TagFetch::Auto {
		auto_follow_tags(
			repo,
			&advertised,
			&positive,
			&negative,
			&reflog,
			&mut updated,
		)
		.await?;
	}
	// Prune (git's `--prune`): delete every tracking ref under a wildcard refspec's destination namespace
	// that no advertised ref maps to. `claimed` holds exactly the destinations some advertised,
	// non-excluded ref mapped to during planning, so a covered tracking ref absent from it is stale — its
	// upstream branch was deleted. A CAS on the resolved oid keeps the delete from racing a concurrent
	// move. Only wildcard destinations own a namespace to prune; an exact/source-only refspec prunes
	// nothing.
	let mut pruned = Vec::new();
	if prune {
		let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
		for spec in &positive {
			let Some(prefix) = spec.destination_glob_prefix() else {
				continue;
			};
			for (tracking, _) in repo.refs().list(prefix).await? {
				if claimed.contains_key(tracking.as_str())
					|| !spec.covers_destination(&tracking)
					|| !seen.insert(tracking.clone())
				{
					continue;
				}
				let current = repo.refs().resolve(&tracking).await?;
				repo
					.refs()
					.delete_ref(&tracking, current, ReflogIntent::Skip)
					.await?;
				pruned.push(tracking);
			}
		}
	}

	Ok(FetchOutcome {
		updated,
		rejected,
		pruned,
	})
}

/// The fetch's fatal, structural errors — checked before any download so a fetch that fails persists no
/// state (neither objects nor a `.git/shallow` boundary): an exact refspec source the remote does not
/// advertise (git's `couldn't find remote ref …`), two different sources mapping onto one tracking ref,
/// and a refspec mapping onto the checked-out branch when the caller does not permit it (`update_head_ok`
/// is set only by `pull`). None of these need the fetched objects; the plan loop in [`fetch`] re-checks
/// them per object once the pack is in hand.
fn validate_fetch_selection<H: HashAlgorithm>(
	advertised: &Advertised<H>,
	positive: &[&Refspec],
	negative: &[&Refspec],
	checked_out: Option<&str>,
	update_head_ok: bool,
	checkouts: &[(String, String)],
) -> Result<()> {
	for spec in positive {
		if let Some(source) = spec.exact_source()
			&& !advertised.refs.iter().any(|(name, _)| name == source)
		{
			bail!("couldn't find remote ref {source}");
		}
	}
	let mut claimed: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
	for (name, _) in &advertised.refs {
		if negative.iter().any(|spec| spec.excludes(name)) {
			continue;
		}
		for spec in positive {
			let Some(tracking) = spec.destination(name) else {
				continue;
			};
			if let Some(&other) = claimed.get(tracking.as_str())
				&& other != name.as_str()
			{
				bail!("cannot fetch both {other} and {name} to {tracking}");
			}
			// A branch checked out in any worktree can't be fetched into directly (git's message names
			// the worktree's path). The sole exception is the current worktree's own branch under
			// `update_head_ok` (`pull`), which advances it via its merge step; every other worktree's
			// branch is refused even then, since a merge advances only the current HEAD.
			if let Some((_, path)) = checkouts.iter().find(|(b, _)| b == tracking.as_str()) {
				let is_current = checked_out == Some(tracking.as_str());
				if !(is_current && update_head_ok) {
					bail!("refusing to fetch into branch '{tracking}' checked out at '{path}'");
				}
			} else if checked_out == Some(tracking.as_str()) && !update_head_ok {
				// Fallback for a caller that supplies no checkout paths (the in-component fetch, which
				// can't see worktree paths through its descriptors): still refuse the current branch here,
				// before `download`, so a failed fetch performs no network / object-store side effects.
				bail!("refusing to fetch into branch '{tracking}' checked out in the work tree");
			}
			claimed.insert(tracking, name.as_str());
		}
	}
	Ok(())
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
	reflog: &Option<FetchReflog<'_>>,
	updated: &mut Vec<(String, ObjectId<H>)>,
) -> Result<()> {
	// Candidate tags: advertised `refs/tags/*` not already present locally (git leaves existing tags
	// alone in auto mode). If none, skip the reachability walk entirely. A shallow fetch may advertise a
	// tag whose object lies outside the fetched boundary (so it was not downloaded); that tag cannot be
	// peeled or followed, so drop it here rather than fail reading an absent object below.
	let mut candidates = Vec::new();
	for (name, oid) in &advertised.refs {
		if name.starts_with("refs/tags/")
			&& repo.refs().resolve(name).await?.is_none()
			&& repo.objects().exists_object(oid).await?
		{
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

	// An auto-followed tag is always a fresh `refs/tags/*` create, so git words it `storing tag`. That
	// namespace is unlogged by default, so `update_ref`'s gating drops the entry unless
	// `core.logAllRefUpdates=always` (or an existing tag reflog) — exactly when git records it too.
	let message = reflog
		.as_ref()
		.map(|r| format!("{}: storing tag", r.action));
	for (name, oid) in candidates {
		// Peel through any tag chain to the object the tag ultimately names (commit / tree / blob).
		let target = peel_tag_target(repo, oid).await?;
		if closure.contains(&target) {
			let intent = match (reflog, &message) {
				(Some(r), Some(msg)) => ReflogIntent::Log {
					committer: r.committer,
					message: msg,
				},
				_ => ReflogIntent::Skip,
			};
			repo.refs().update_ref(&name, oid, None, intent).await?;
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
	let config = repo.effective_config().await?;
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
/// origin, and check out `HEAD`. The ref advertisement and the pack both come over `connection` (an
/// [`HttpConnection`](gitana_remote::HttpConnection) for a Smart HTTP remote, an
/// [`SshConnection`](gitana_remote::SshConnection) for SSH), so the same clone drives either transport.
/// The origin is persisted through `repo`'s file store (no ambient filesystem access), so this runs over
/// any [`FileStore`] — a local checkout or the wasm descriptor backend.
///
/// `persist_url` is the URL written to `remote.origin.url` — the caller resolves it (git records the
/// pre-`insteadOf` argument, and the CLI redacts any password). `reflog` supplies the committer and
/// verbatim source URL for the `clone: from <url>` entries git records on `HEAD` and the checked-out
/// branch; `None` (the in-component clone, with no configured identity) writes no reflog.
pub async fn clone<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	connection: &mut impl Connection,
	repo: Repository<F, H>,
	work: W,
	deepen: &Deepen,
	reflog: Option<CloneReflog<'_>>,
	persist_url: &str,
	sparse: bool,
) -> Result<()> {
	repo.init().await?;

	let advertised = parse_advertisement::<H>(connection.advertisement())?;
	// A shallow clone deepens from the branch tips and `HEAD` (see `shallow_wants`); a full clone ignores
	// these roots and wants every advertised ref. A shallow clone requests reachable tags (`include-tag`)
	// so tags pointing into the fetched history are preserved.
	let roots = shallow_wants(&advertised);
	download_clone(
		connection,
		&repo,
		&advertised,
		deepen,
		&roots,
		!deepen.is_empty(),
	)
	.await?;

	// Point HEAD at the remote's default branch *before* recreating the refs, so that when the loop
	// writes that branch, `update_ref`'s "split HEAD update" cascades clone's reflog into `logs/HEAD`
	// as a creation (old = zero), exactly as git records it. Retargeting an unborn branch logs nothing
	// (and we pass Skip regardless), so no stray HEAD entry precedes the branch write.
	let head_target = advertised
		.head_target
		.clone()
		.unwrap_or_else(|| "refs/heads/main".to_owned());
	repo
		.refs()
		.set_head_symbolic(&head_target, ReflogIntent::Skip)
		.await?;

	// Recreate the refs and HEAD locally. A shallow clone fetches only branch history (see
	// `download`), so an advertised ref whose target is outside that closure — e.g. a tag on the
	// truncated history — is skipped rather than recreated as a dangling ref pointing at a missing
	// object. A full clone holds the whole closure, so nothing is skipped there.
	let clone_reflog = reflog.map(|r| (r.committer, format!("clone: from {}", r.url)));
	let shallow = !deepen.is_empty();
	for (name, oid) in &advertised.refs {
		if name.starts_with("refs/") {
			if shallow && !repo.objects().exists_object(oid).await? {
				continue;
			}
			// Only the checked-out branch (HEAD's target) carries clone's reflog — writing it cascades the
			// entry into `logs/HEAD`. The other refs gta recreates here stand in for git's remote-tracking
			// refs, which git leaves unlogged, so they pass Skip.
			let intent = match &clone_reflog {
				Some((c, msg)) if *name == head_target => ReflogIntent::Log {
					committer: c,
					message: msg,
				},
				_ => ReflogIntent::Skip,
			};
			repo.refs().update_ref(name, *oid, None, intent).await?;
		}
	}
	// Persist the remote (the caller-resolved URL, scheme-agnostic) through the file store.
	gitana_remote::save_remote_origin(repo.objects().file_store(), persist_url).await?;

	// Populate the working tree from HEAD (if the repo had any commits).
	let head = repo.refs().resolve_head().await?;
	// The `git_dir` a `WorkTree` carries is inert — the index and all git-dir files route through the
	// `FileStore` — so a placeholder path suffices, as elsewhere in the worktree layer.
	let worktree = WorkTree::new(repo, work, "");
	// `--sparse` (git's clone --sparse): initialise cone sparse-checkout with the default set (root files
	// only) BEFORE any checkout, so only the in-cone paths are ever written — rather than materialising the
	// whole tree and removing most of it. Written even for an empty remote (git's clone --sparse still lays
	// down the config + pattern file so a later first checkout is sparse); with a HEAD the subsequent
	// checkout honours the patterns just written (the index is still empty, so this reapply is a no-op).
	if sparse {
		worktree
			.apply_sparse_set(&gitana_worktree::SparseSet::Cone(Vec::new()))
			.await?;
	}
	if let Some(commit) = head {
		let tree = worktree.repository().commit_tree(commit).await?;
		worktree.checkout(tree, true, None).await?;
	}
	Ok(())
}

/// Download a **clone**'s objects over a [`Connection`] (the single-round counterpart of [`download`],
/// which negotiates `have`s for an incremental fetch over the stateless-HTTP path). A full clone wants
/// every advertised ref; a shallow one deepens from `deepen_roots`. `include_tag` requests reachable
/// annotated tags for a shallow clone.
async fn download_clone<F: FileStore, H: HashAlgorithm>(
	connection: &mut impl Connection,
	repo: &Repository<F, H>,
	advertised: &Advertised<H>,
	deepen: &Deepen,
	deepen_roots: &[ObjectId<H>],
	include_tag: bool,
) -> Result<()> {
	ensure_deepen_supported(advertised, deepen)?;
	let wants = if deepen.is_empty() {
		gitana_remote::advertised_oids(advertised)
	} else {
		let mut wants = deepen_roots.to_vec();
		wants.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
		wants.dedup();
		wants
	};
	gitana_remote::download_clone_pack(connection, repo, &wants, deepen, include_tag).await
}

/// Download the objects reachable from the advertised tips that `haves` do not already cover, writing
/// them into `repo`. A normal fetch (empty `deepen`) wants every advertised ref; a shallow one deepens
/// from `deepen_roots` — each root becomes its own shallow boundary. The caller chooses those roots:
/// `clone` passes the branch tips and `HEAD` ([`shallow_wants`]); `fetch` passes the refs its refspecs
/// select (so a negatively-excluded or unrequested ref is neither fetched nor marked shallow).
///
/// `include_tag` requests reachable annotated tags for a shallow fetch (git's `include-tag`); a caller
/// disabling tags (`--no-tags`) passes `false`.
#[allow(clippy::too_many_arguments)]
async fn download<F: FileStore, H: HashAlgorithm>(
	fetcher: &mut impl PackFetcher,
	repo: &Repository<F, H>,
	advertised: &Advertised<H>,
	haves: &[ObjectId<H>],
	deepen: &Deepen,
	deepen_roots: &[ObjectId<H>],
	include_tag: bool,
) -> Result<()> {
	ensure_deepen_supported(advertised, deepen)?;
	let wants = if deepen.is_empty() {
		gitana_remote::advertised_oids(advertised)
	} else {
		let mut wants = deepen_roots.to_vec();
		wants.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
		wants.dedup();
		wants
	};
	fetcher
		.fetch_pack(repo, &wants, haves, deepen, include_tag)
		.await?;
	Ok(())
}

/// Fail a shallow request the server cannot honor: if the advertisement lacks the matching capability
/// (`shallow` for `--depth`, `deepen-since` / `deepen-not` for the date/exclude forms, `deepen-relative`
/// for `--deepen`), the server would silently ignore the directive and return a different (or full) pack
/// — so reject it up front, as git does ("Server does not support shallow requests"), rather than
/// pretend the request was honored.
fn ensure_deepen_supported<H: HashAlgorithm>(
	advertised: &Advertised<H>,
	deepen: &Deepen,
) -> Result<()> {
	if deepen.is_empty() {
		return Ok(());
	}
	if deepen.depth.is_some() && !advertised.supports("shallow") {
		bail!("the remote does not support shallow clones (no `shallow` capability advertised)");
	}
	// `--deepen` (relative) needs `deepen-relative`: without it the server would read the `deepen N` as an
	// absolute depth from the tips, silently producing a different boundary than the requested relative one.
	if deepen.relative && !advertised.supports("deepen-relative") {
		bail!("the remote does not support --deepen (no `deepen-relative` capability advertised)");
	}
	if deepen.since.is_some() && !advertised.supports("deepen-since") {
		bail!("the remote does not support --shallow-since (no `deepen-since` capability advertised)");
	}
	if !deepen.not.is_empty() && !advertised.supports("deepen-not") {
		bail!("the remote does not support --shallow-exclude (no `deepen-not` capability advertised)");
	}
	Ok(())
}

/// The base deepen roots for a shallow clone/fetch: the branch tips (`refs/heads/*`) and `HEAD`.
/// Advertised tags and other refs are excluded *here* — deepening from an old tag would pull history the
/// `--depth` / `--shallow-exclude` request is meant to prune. Tags pointing *into* the fetched history
/// are still preserved: the request sets git's `include-tag`, so the server appends the reachable
/// annotated tag objects, and [`clone`] recreates each ref whose target then lands in the closure.
/// (`fetch` additionally deepens from any *explicitly* requested tag/ref — `--tags`, a tag refspec — via
/// `download`'s `extra_roots`, so those are fetched as their own shallow roots even outside branches.)
fn shallow_wants<H: HashAlgorithm>(advertised: &Advertised<H>) -> Vec<ObjectId<H>> {
	let mut oids: Vec<ObjectId<H>> = advertised
		.refs
		.iter()
		.filter(|(name, _)| name == "HEAD" || name.starts_with("refs/heads/"))
		.map(|(_, oid)| *oid)
		.collect();
	oids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
	oids.dedup();
	oids
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
#[allow(clippy::too_many_arguments)]
pub async fn push<F: FileStore, H: HashAlgorithm>(
	connection: &mut impl Connection,
	repo: &Repository<F, H>,
	advertisement: &[u8],
	force: bool,
	atomic: bool,
	refspecs: Vec<PushRefspec>,
	tags: PushTags,
) -> Result<PushOutcome> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	ensure_atomic_supported(&advertised, atomic)?;
	let planned = plan_push(repo, &advertised, refspecs, force, tags).await?;
	if planned.is_empty() {
		// Nothing to push (already up to date): still finalise the session — an SSH connection owes the
		// terminating flush and a nonzero transport exit must surface (a no-op for stateless HTTP).
		connection.finish().await?;
		return Ok(PushOutcome {
			results: Vec::new(),
			signed: false,
		});
	}
	let updates: Vec<RefUpdate<H>> = planned.iter().map(|p| p.update.clone()).collect();
	let pack = pack_for(repo, &advertised, &planned).await?;
	let request = build_receive_pack_request(&updates, atomic, &pack);
	send_receive_pack(connection, request).await?;
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
#[allow(clippy::too_many_arguments)]
pub async fn push_signed<F: FileStore, H: HashAlgorithm, S: Signer>(
	connection: &mut impl Connection,
	repo: &Repository<F, H>,
	pushee: &str,
	advertisement: &[u8],
	force: bool,
	atomic: bool,
	refspecs: Vec<PushRefspec>,
	tags: PushTags,
	pusher: impl AsyncFnOnce() -> Result<String>,
	signer: &S,
) -> Result<PushOutcome> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	ensure_atomic_supported(&advertised, atomic)?;
	let planned = plan_push(repo, &advertised, refspecs, force, tags).await?;
	if planned.is_empty() {
		// Nothing to push: finalise the session (SSH flush + exit status) before returning up-to-date.
		connection.finish().await?;
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
	let mut cert = build_cert(pushee, pusher().await?, nonce, &updates);
	// The signer emits an SSHSIG armor (git's `git` namespace) over the certificate body — exactly what
	// receive-pack verifies via `verify_sshsig(cert.payload(), cert.signature, keys, "git")`.
	cert.signature = signer.sign(&cert.payload()).await?;
	let pack = pack_for(repo, &advertised, &planned).await?;
	let request = build_push_cert(&cert, &push_caps::<H>(atomic), &pack);
	send_receive_pack(connection, request).await?;
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
	// Resolve bare-name sources (`push origin v1`) against the local refs first — rewriting a tag-only
	// name to `refs/tags/v1:refs/tags/v1` — so tag expansion below sees the real destinations (its dedup
	// and follow-tag reachability would otherwise key off the branch-defaulted `refs/heads/v1`).
	let mut dwimmed = Vec::with_capacity(base.len());
	for spec in base {
		dwimmed.push(dwim_bare_source(repo, spec).await?);
	}
	let base = dwimmed;
	// Expand `--tags` / `--follow-tags` into additional `refs/tags/*` refspecs (see `tag_refspecs`).
	let tag_specs = tag_refspecs(repo, advertised, &base, tags).await?;
	let refspecs: Vec<PushRefspec> = base.into_iter().chain(tag_specs).collect();
	let mut planned = Vec::new();
	let mut seen_dsts = std::collections::HashSet::new();
	for spec in refspecs {
		let forced = force || spec.force;
		// A `HEAD` destination (git's `push origin HEAD` shorthand) means the current branch's ref; a
		// bare deletion target (`:v1`) resolves against the remote's advertised refs (branch vs tag).
		let dst = resolve_destination(repo, advertised, &spec).await?;
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
				// force, even a fast-forward — a tag is a fixed name, not a moving branch tip.
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
		base_dsts.insert(resolve_destination(repo, advertised, spec).await?);
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
				dst_bare: false,
				src_bare: false,
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

/// Resolve a push destination: the literal `HEAD` becomes the current branch's ref; a bare deletion
/// target is DWIM'd against the remote's advertised refs; any other name is already a full ref.
async fn resolve_destination<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	advertised: &Advertised<H>,
	spec: &PushRefspec,
) -> Result<String> {
	if spec.dst == "HEAD" {
		return match repo.refs().read_head().await? {
			HeadState::Symbolic(branch) => Ok(branch),
			HeadState::Detached(_) => {
				bail!("cannot push to `HEAD` from a detached HEAD; use an explicit destination ref")
			}
		};
	}
	// A deletion of a bare name (`:v1`) resolves against the remote's refs: git deletes an existing
	// `refs/tags/v1` rather than a nonexistent `refs/heads/v1`. The branch default (`spec.dst`) stands
	// unless the remote has only the tag; having both is ambiguous.
	if spec.src.is_none() && spec.dst_bare {
		let name = spec
			.dst
			.strip_prefix("refs/heads/")
			.expect("a bare destination is branch-qualified");
		let as_tag = format!("refs/tags/{name}");
		let has_branch = advertised.oid_of(&spec.dst).is_some();
		let has_tag = advertised.oid_of(&as_tag).is_some();
		return match (has_branch, has_tag) {
			(true, true) => bail!(
				"{name} is ambiguous on the remote (both {} and {as_tag}); delete with a full ref name",
				spec.dst
			),
			(false, true) => Ok(as_tag),
			// Branch present, or neither (the deletion below then reports the missing ref).
			_ => Ok(spec.dst.clone()),
		};
	}
	Ok(spec.dst.clone())
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
	if let Ok(config) = repo.effective_config().await {
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
		dst_bare: false,
		src_bare: false,
	}])
}

/// DWIM a bare-name push source against the *local* refs: `push origin v1` pushes an existing local
/// `refs/tags/v1` (into `refs/tags/v1`) rather than a nonexistent `refs/heads/v1`. Only a bare `<name>`
/// push (`src_bare`) is rewritten — an explicit source is literal. The branch default stands unless the
/// local repo has only the tag; having both a branch and a tag by that name is ambiguous (as in git).
async fn dwim_bare_source<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	spec: PushRefspec,
) -> Result<PushRefspec> {
	if !spec.src_bare {
		return Ok(spec);
	}
	let src = spec
		.src
		.as_deref()
		.expect("a bare source push has a source");
	let name = src
		.strip_prefix("refs/heads/")
		.expect("a bare source is branch-qualified");
	let as_tag = format!("refs/tags/{name}");
	let has_branch = repo.refs().resolve(src).await?.is_some();
	let has_tag = repo.refs().resolve(&as_tag).await?.is_some();
	match (has_branch, has_tag) {
		(true, true) => {
			bail!("{name} is ambiguous locally (both {src} and {as_tag}); push with a full ref name")
		}
		// Only the tag exists: push it into the same-named remote tag. `src_bare` is cleared — the name
		// is now resolved to a full ref, so this is idempotent if ever re-applied.
		(false, true) => Ok(PushRefspec {
			src: Some(as_tag.clone()),
			dst: as_tag,
			src_bare: false,
			..spec
		}),
		// Branch present, or neither (resolve_source then reports the missing ref).
		_ => Ok(spec),
	}
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
	// Send a thin pack: deltas against the advertised tips the remote already has. Both
	// stock git and gitana's receive-pack complete an incoming thin pack before storing.
	Ok(build_pack_thin(repo, &wants, &haves).await?)
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
async fn send_receive_pack(connection: &mut impl Connection, request: Vec<u8>) -> Result<()> {
	let response = connection.exchange(request).await?;
	parse_report_status(&response)?;
	connection.finish().await
}

/// Build a push certificate carrying one command per `update`, with an empty `signature`: the caller
/// signs [`PushCert::payload`] and fills it in. Each command's `old`/`new` are the ref's before/after
/// values — a `None` becomes the all-zero id, so a create is `old: None` and a delete is `new: None`.
fn build_cert<H: HashAlgorithm>(
	pushee: &str,
	pusher: String,
	nonce: String,
	updates: &[RefUpdate<H>],
) -> PushCert {
	let zero = "0".repeat(H::RAW_LEN * 2);
	let hex = |id: Option<ObjectId<H>>| id.map_or_else(|| zero.clone(), |oid| oid.to_hex());
	PushCert {
		version: "0.1".to_owned(),
		pusher,
		pushee: pushee.to_owned(),
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

/// Capabilities echoed on the push request's first line / cert marker, for hash `H`. `atomic`
/// requests git's all-or-nothing `--atomic` capability.
fn push_caps<H: HashAlgorithm>(atomic: bool) -> String {
	let atomic = if atomic { " atomic" } else { "" };
	format!("report-status{atomic} object-format={}", H::NAME)
}

/// Fail an `--atomic` push the server cannot honor: git requires the receiving end to advertise the
/// `atomic` capability, and errors out rather than silently applying the refs per-ref.
fn ensure_atomic_supported<H: HashAlgorithm>(
	advertised: &Advertised<H>,
	atomic: bool,
) -> Result<()> {
	if atomic && !advertised.supports("atomic") {
		bail!("the receiving end does not support --atomic push");
	}
	Ok(())
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

	#[test]
	fn deepen_requires_matching_server_capability() {
		use gitana_object::Sha256;

		let full: Advertised<Sha256> = Advertised {
			capabilities: vec![
				"shallow".to_owned(),
				"deepen-since".to_owned(),
				"deepen-not".to_owned(),
			],
			..Default::default()
		};
		let none = Advertised::<Sha256>::default();

		// An empty deepen (a normal fetch) never needs a capability.
		assert!(ensure_deepen_supported(&none, &Deepen::default()).is_ok());
		// Each shallow directive requires its own advertised capability.
		let depth = Deepen {
			depth: Some(1),
			..Default::default()
		};
		assert!(ensure_deepen_supported(&none, &depth).is_err());
		assert!(ensure_deepen_supported(&full, &depth).is_ok());
		let since = Deepen {
			since: Some(1),
			..Default::default()
		};
		assert!(ensure_deepen_supported(&none, &since).is_err());
		let not = Deepen {
			not: vec!["main".to_owned()],
			..Default::default()
		};
		assert!(ensure_deepen_supported(&none, &not).is_err());
		// `--deepen` (relative) additionally needs the `deepen-relative` capability: a server that offers
		// `shallow` but not `deepen-relative` cannot honor it.
		let relative = Deepen {
			depth: Some(1),
			relative: true,
			..Default::default()
		};
		let shallow_only: Advertised<Sha256> = Advertised {
			capabilities: vec!["shallow".to_owned()],
			..Default::default()
		};
		assert!(ensure_deepen_supported(&shallow_only, &relative).is_err());
		let with_relative: Advertised<Sha256> = Advertised {
			capabilities: vec!["shallow".to_owned(), "deepen-relative".to_owned()],
			..Default::default()
		};
		assert!(ensure_deepen_supported(&with_relative, &relative).is_ok());
	}

	#[test]
	fn atomic_requires_matching_server_capability() {
		use gitana_object::Sha256;

		let with: Advertised<Sha256> = Advertised {
			capabilities: vec!["report-status".to_owned(), "atomic".to_owned()],
			..Default::default()
		};
		let without: Advertised<Sha256> = Advertised {
			capabilities: vec!["report-status".to_owned()],
			..Default::default()
		};
		// A default (non-atomic) push never needs the capability.
		assert!(ensure_atomic_supported(&without, false).is_ok());
		// `--atomic` requires the server to advertise `atomic`, else it errors rather than degrading to
		// a per-ref push.
		assert!(ensure_atomic_supported(&without, true).is_err());
		assert!(ensure_atomic_supported(&with, true).is_ok());
		// The client echoes the `atomic` token only when requested.
		assert!(push_caps::<Sha256>(true).contains("atomic"));
		assert!(!push_caps::<Sha256>(false).contains("atomic"));
	}

	/// The pushee URL the certificate tests bind to (the value of the parsed origin's `url`).
	const PUSHEE: &str = "http://host/acme/app";

	/// A [`Connection`] double that records the single request exchanged and answers with a success
	/// `report-status` (`unpack ok`), so a push completes without a real server.
	struct CapturingConnection {
		posted: RefCell<Option<Vec<u8>>>,
	}

	impl CapturingConnection {
		fn new() -> Self {
			Self {
				posted: RefCell::new(None),
			}
		}
	}

	impl Connection for CapturingConnection {
		fn advertisement(&self) -> &[u8] {
			unreachable!("push passes the advertisement in; it does not read it from the connection")
		}

		async fn exchange(&mut self, body: Vec<u8>) -> Result<Vec<u8>> {
			*self.posted.borrow_mut() = Some(body);
			let mut report = Vec::new();
			write_pkt(&mut report, b"unpack ok\n").unwrap();
			write_flush(&mut report);
			Ok(report)
		}

		async fn finish(&mut self) -> Result<()> {
			Ok(())
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

		let signer = TestSigner::new(7);
		let public_line = signer.public_line();
		let mut conn = CapturingConnection::new();

		let outcome = push_signed(
			&mut conn,
			wt.repository(),
			PUSHEE,
			&advertisement,
			false,
			false,
			vec![],
			PushTags::None,
			async || Ok("Dev <dev@x.test> 1700000000 +0000".to_owned()),
			&signer,
		)
		.await
		.unwrap();
		assert!(outcome.signed && outcome.results.len() == 1 && !outcome.results[0].deleted);

		// The exchanged request is a push certificate binding this create to the server's nonce.
		let request = conn.posted.into_inner().expect("a request was exchanged");
		let cert = peek_push_cert(&request).expect("a signed push-cert request");
		assert_eq!(cert.nonce, nonce);
		assert_eq!(cert.pushee, PUSHEE);
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
		let signer = TestSigner::new(7);
		let public_line = signer.public_line();
		let mut conn = CapturingConnection::new();

		let outcome = push_signed(
			&mut conn,
			wt.repository(),
			PUSHEE,
			&advertisement,
			false,
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

		// The exchanged request is a push certificate whose command deletes the ref (new = zero, old = tip).
		let request = conn.posted.into_inner().expect("a request was exchanged");
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

		let mut conn = CapturingConnection::new();
		let result = push_signed(
			&mut conn,
			wt.repository(),
			PUSHEE,
			&advertisement,
			false,
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
		assert!(conn.posted.into_inner().is_none(), "nothing was exchanged");
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
			.update_ref("refs/heads/dev", tip, None, ReflogIntent::Skip)
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

		let mut conn = CapturingConnection::new();
		let result = push(
			&mut conn,
			wt.repository(),
			&advertisement,
			false,
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
		assert!(conn.posted.into_inner().is_none(), "nothing was exchanged");
	}
}
