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
use gitana_remote::{HttpTransport, Origin, RECEIVE_PACK_REQUEST, Refspec};
use gitana_repository::{HeadState, Repository};
use gitana_worktree::WorkTree;

use crate::Signer;

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
) -> Result<FetchOutcome<H>> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let haves = gitana_remote::local_haves(repo).await?;
	download(transport, repo, origin, &advertised, &haves).await?;

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

/// Push `HEAD`'s branch to `origin` over an unsigned receive-pack request (or, with `delete`, remove a
/// remote branch). `advertisement` is the already-fetched `git-receive-pack` `GET /info/refs` body.
/// For a signed push (`gta push --signed`), see [`push_signed`].
pub async fn push<F: FileStore, H: HashAlgorithm>(
	transport: &impl HttpTransport,
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	force: bool,
	delete: Option<String>,
) -> Result<PushOutcome> {
	let advertised = parse_advertisement::<H>(advertisement)?;

	if let Some(target) = delete {
		return delete_ref(transport, origin, &advertised, &target).await;
	}

	let Some(plan) = prepare_branch_push(repo, &advertised, force).await? else {
		return Ok(PushOutcome::UpToDate);
	};
	let update = RefUpdate {
		old: plan.remote_old,
		new: Some(plan.local_tip),
		name: plan.branch.clone(),
	};
	let request = build_receive_pack_request(std::slice::from_ref(&update), &plan.pack);
	send_receive_pack(transport, origin, request).await?;
	Ok(PushOutcome::Pushed {
		branch: plan.branch,
		signed: false,
		forced: force,
	})
}

/// Push `HEAD`'s branch to `origin` with a signed push certificate (`gta push --signed`). Otherwise
/// like [`push`]. `pusher` resolves the certificate's pusher line and `signer` signs the certificate
/// body — both invoked only after confirming the server offers push-cert, so an unconfigured identity
/// or an unresolvable signing key does not mask "the server does not accept signed pushes".
pub async fn push_signed<F: FileStore, H: HashAlgorithm, S: Signer>(
	transport: &impl HttpTransport,
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	force: bool,
	pusher: impl AsyncFnOnce() -> Result<String>,
	signer: &S,
) -> Result<PushOutcome> {
	let advertised = parse_advertisement::<H>(advertisement)?;

	let Some(plan) = prepare_branch_push(repo, &advertised, force).await? else {
		return Ok(PushOutcome::UpToDate);
	};
	let nonce = advertised
		.push_cert_nonce
		.clone()
		.context("the server does not accept signed pushes")?;
	let mut cert = build_cert(
		origin,
		pusher().await?,
		nonce,
		plan.remote_old,
		plan.local_tip,
		&plan.branch,
	);
	// The signer emits an SSHSIG armor (git's `git` namespace) over the certificate body — exactly what
	// receive-pack verifies via `verify_sshsig(cert.payload(), cert.signature, keys, "git")`.
	cert.signature = signer.sign(&cert.payload()).await?;
	let request = build_push_cert(&cert, &push_caps::<H>(), &plan.pack);
	send_receive_pack(transport, origin, request).await?;
	Ok(PushOutcome::Pushed {
		branch: plan.branch,
		signed: true,
		forced: force,
	})
}

/// A planned branch push: the ref-update coordinates and the pack the remote lacks, shared by [`push`]
/// and [`push_signed`]. `prepare_branch_push` returns `None` when the remote already has the tip.
struct BranchPush<H: HashAlgorithm> {
	branch: String,
	remote_old: Option<ObjectId<H>>,
	local_tip: ObjectId<H>,
	pack: Vec<u8>,
}

/// Resolve `HEAD`'s branch and tip against the advertised refs, enforce the fast-forward rule (unless
/// `force`), and pack the objects the remote lacks. Returns `None` when the remote already has the tip
/// (nothing to send). Shared preamble of the signed and unsigned pushes so both apply the same guards.
async fn prepare_branch_push<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	advertised: &Advertised<H>,
	force: bool,
) -> Result<Option<BranchPush<H>>> {
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
		return Ok(None);
	}

	// Without `--force`, refuse a non-fast-forward before sending anything: the remote tip must be an
	// ancestor of the local tip (git's client-side check). A create (no remote tip) is always a
	// fast-forward. Relying on the server to reject is not enough — a server configured to permit
	// rewrites would otherwise silently overwrite the remote branch.
	if !force
		&& let Some(remote_old) = remote_old
		&& !repo.is_ancestor(remote_old, local_tip).await?
	{
		bail!(
			"updates were rejected because the remote contains work that you do not have locally; \
			 integrate the remote changes (e.g. fetch) before pushing again, or use --force"
		);
	}

	// Pack the objects the remote lacks (reachable from the tip, minus its refs).
	let haves = gitana_remote::advertised_oids(advertised);
	let pack = build_pack(repo, &[local_tip], &haves).await?;
	Ok(Some(BranchPush {
		branch,
		remote_old,
		local_tip,
		pack,
	}))
}

/// Send a delete-ref command for `target` (a branch name or full ref) to the remote.
async fn delete_ref<H: HashAlgorithm>(
	transport: &impl HttpTransport,
	origin: &Origin,
	advertised: &Advertised<H>,
	target: &str,
) -> Result<PushOutcome> {
	let refname = normalize_branch(target);
	let remote = advertised
		.oid_of(&refname)
		.with_context(|| format!("the remote has no {refname}"))?;
	let update = RefUpdate {
		old: Some(remote),
		new: None,
		name: refname.clone(),
	};
	let request = build_receive_pack_request(std::slice::from_ref(&update), &[]);
	send_receive_pack(transport, origin, request).await?;
	Ok(PushOutcome::Deleted { refname })
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

/// Build a push certificate for a single-branch update, with an empty `signature`: the caller signs
/// [`PushCert::payload`] and fills it in. The payload (pusher, pushee, nonce, and command) is complete.
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
			async || Ok("Dev <dev@x.test> 1700000000 +0000".to_owned()),
			&signer,
		)
		.await
		.unwrap();
		assert!(matches!(outcome, PushOutcome::Pushed { signed: true, .. }));

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
}
