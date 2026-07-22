//! Remote operations — `fetch`, `push`, and `clone` over the in-guest `wasi:http` transport.
//!
//! These reuse `gitana-porcelain`'s remote composites unchanged, injecting the component's
//! [`WasiHttpTransport`] (an `HttpClient`) wrapped in an [`AuthTransport`] over the host-granted
//! credential capability ([`WasiCredentialProvider`]). The advertisement GET and the pack POST both
//! flow through `wasi:http`; nothing here reaches for `reqwest`. A remote that answers `401
//! WWW-Authenticate: Basic` is authenticated with a credential the host resolves over the WIT
//! `credentials` import (git's unauth-first, retry-once flow); an anonymous remote is unaffected — the
//! `AuthTransport` sends nothing pre-emptively, exactly as the previous unauthenticated wrapper did.

use gitana_file_store_local::{DescriptorWorkDir, WorktreeFileStore};
use gitana_object::HashAlgorithm;
use gitana_object_store::ObjectStore;
use gitana_porcelain::Deepen;
use gitana_remote::{
	AuthTransport, HttpConnection, HttpPackFetcher, Origin, RECEIVE_PACK_REQUEST, UPLOAD_PACK_REQUEST,
};
use gitana_repository::Repository;

use super::WasiCredentialProvider;
use crate::bindings::exports::gitana::repo::porcelain::{
	FetchOutcome, HashKind, PushOutcome as WitPushOutcome, PushSummary, RefEntry, RepoError,
};
use crate::wasi_http_transport::WasiHttpTransport;

/// The component's authenticating transport: the `wasi:http` client wrapped with the host credential
/// capability. Held across a whole operation so an accepted credential is cached and re-sent on later
/// requests (the advertisement `GET` then the pack `POST`) rather than re-challenged — matching git.
pub(crate) type ComponentTransport = AuthTransport<WasiHttpTransport, WasiCredentialProvider>;

/// Build the [`ComponentTransport`] for `origin`: keyed on the userinfo-stripped repository URL and
/// seeded with any userinfo the URL carried (a full `user:pass` is the first credential tried on a Basic
/// `401`; a bare username only pre-fills the request). Mirrors the CLI's `transport_for`, minus the git
/// config. One transport is built per operation and threaded through its every request, so the
/// credential the host resolves on the first `401` is reused — a `fill` that only answers once (a prompt
/// or one-shot token) still authenticates the whole clone/fetch/push.
pub(crate) fn auth_transport(origin: &Origin) -> ComponentTransport {
	AuthTransport::with_userinfo(
		WasiHttpTransport,
		WasiCredentialProvider::new(),
		origin.url.clone(),
		origin.username.clone(),
		origin.password.clone(),
	)
}

/// Fetch from the remote at `url` into `repo`, returning the tracking-ref outcome. The advertised
/// object-format is checked against `repo`'s before any objects are downloaded.
pub(crate) async fn fetch<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	url: &str,
) -> Result<FetchOutcome, RepoError> {
	let origin = Origin::parse(url).map_err(|e| RepoError::Invalid(e.to_string()))?;
	let transport = auth_transport(&origin);

	let advertisement = gitana_remote::fetch_advertisement(&transport, &origin, "git-upload-pack")
		.await
		.map_err(remote_error)?;
	let remote = gitana_remote::negotiated_kind(&advertisement).map_err(remote_error)?;
	if H::NAME != remote.name() {
		return Err(RepoError::Invalid(format!(
			"remote object-format is {}, but the local repository is {}",
			remote.name(),
			H::NAME
		)));
	}

	// The component sees a single worktree through its capability descriptors, so it has no view of any
	// sibling linked worktrees; the current HEAD is still guarded inside the porcelain. Fetch negotiates
	// over the `wasi:http` transport (stateless-RPC); SSH is native-only.
	let mut fetcher = HttpPackFetcher::new(&transport, &origin);
	let outcome = gitana_porcelain::fetch(
		&mut fetcher,
		repo,
		&advertisement,
		false,
		gitana_porcelain::TagFetch::Auto,
		&gitana_porcelain::Deepen::default(),
		&[],
		// The component resolves no committer identity through its descriptors, so its tracking-ref
		// updates go unlogged (a deferred follow-up, like the plumbing `update_ref` exports).
		None,
	)
	.await
	.map_err(remote_error)?;
	Ok(FetchOutcome {
		updated: outcome
			.updated
			.into_iter()
			.map(|(name, id)| RefEntry {
				name,
				id: id.to_hex(),
			})
			.collect(),
		rejected: outcome.rejected,
	})
}

/// Push `HEAD`'s branch to the remote at `url` (or, with `delete`, remove a remote branch),
/// returning the outcome. The advertised object-format is checked against `repo`'s first. This is an
/// unsigned push: certificate signing shells out to `ssh-keygen`, which the component has no authority
/// to do, so `gta push --signed` stays on the CLI.
pub(crate) async fn push<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	url: &str,
	force: bool,
	delete: Option<String>,
) -> Result<WitPushOutcome, RepoError> {
	let origin = Origin::parse(url).map_err(|e| RepoError::Invalid(e.to_string()))?;
	let transport = auth_transport(&origin);

	let advertisement = gitana_remote::fetch_advertisement(&transport, &origin, "git-receive-pack")
		.await
		.map_err(remote_error)?;
	let remote = gitana_remote::negotiated_kind(&advertisement).map_err(remote_error)?;
	if H::NAME != remote.name() {
		return Err(RepoError::Invalid(format!(
			"remote object-format is {}, but the local repository is {}",
			remote.name(),
			H::NAME
		)));
	}

	// The component surface pushes `HEAD`'s branch or deletes one ref — expressed as push refspecs.
	// The push case is an explicit `HEAD` refspec (not an empty list): the porcelain default would
	// honour `remote.origin.push`, which could push a different or multiple refs and break this WIT
	// contract of "push HEAD's branch" (and the single-result mapping below).
	let refspecs = match delete {
		Some(target) => vec![
			gitana_remote::PushRefspec::parse(&format!(":{target}"))
				.map_err(|e| RepoError::Invalid(e.to_string()))?,
		],
		None => vec![
			gitana_remote::PushRefspec::parse("HEAD").map_err(|e| RepoError::Invalid(e.to_string()))?,
		],
	};
	// The component surface pushes a single ref, so atomicity would be a no-op; it is not exposed in
	// the WIT contract. The receive-pack request is one exchange over the `wasi:http` transport (the
	// connection's own advertisement is unused, since push takes it as an argument).
	let mut connection = HttpConnection::new(
		&transport,
		origin.receive_pack(),
		RECEIVE_PACK_REQUEST,
		Vec::new(),
	);
	let outcome = gitana_porcelain::push(
		&mut connection,
		repo,
		&advertisement,
		force,
		false,
		refspecs,
		gitana_porcelain::PushTags::None,
	)
	.await
	.map_err(remote_error)?;

	// Exactly one result in the component case (a branch push or a single delete), or none when the
	// remote was already up to date.
	Ok(match outcome.results.first() {
		None => WitPushOutcome::UpToDate,
		Some(result) if result.deleted => WitPushOutcome::Deleted(result.refname.clone()),
		Some(result) => WitPushOutcome::Pushed(PushSummary {
			branch: result.refname.clone(),
			forced: result.forced,
		}),
	})
}

/// Fetch the remote's ref advertisement and read the object format it advertises — the pre-dispatch
/// step of `clone`, run before any local repository exists to detect a format from. The caller then
/// creates the repository as `HashKind` and hands the advertisement to [`clone`] (no second GET). Takes
/// the operation's [`ComponentTransport`] so the credential this `GET` authenticates is cached and
/// reused by the following pack `POST` in [`clone`], rather than provoking a second `401`/`fill`.
pub(crate) async fn clone_negotiate(
	transport: &ComponentTransport,
	origin: &Origin,
) -> Result<(Vec<u8>, HashKind), RepoError> {
	let advertisement = gitana_remote::fetch_advertisement(transport, origin, "git-upload-pack")
		.await
		.map_err(remote_error)?;
	let kind = match gitana_remote::negotiated_kind(&advertisement).map_err(remote_error)? {
		gitana_object::HashKind::Sha1 => HashKind::Sha1,
		gitana_object::HashKind::Sha256 => HashKind::Sha256,
	};
	Ok((advertisement, kind))
}

/// Clone the remote at `origin` into a fresh checkout backed by `store` (the git directory) and
/// `work` (the working tree), as hash `H`. The caller has already fetched `advertisement` — to
/// negotiate `H` — and laid the git skeleton into `store`; here we run `gitana-porcelain`'s clone
/// (init, download, recreate refs/HEAD, save origin, check out `HEAD`) unchanged. `transport` is the
/// same one [`clone_negotiate`] used, so a credential accepted during negotiation is re-sent here.
pub(crate) async fn clone<H: HashAlgorithm>(
	transport: &ComponentTransport,
	store: WorktreeFileStore,
	work: DescriptorWorkDir,
	origin: &Origin,
	advertisement: &[u8],
) -> Result<(), RepoError> {
	let repo: Repository<WorktreeFileStore, H> = Repository::new(ObjectStore::new(store));
	// Drive the porcelain clone over the connection seam: the caller already fetched the advertisement,
	// and each pack exchange is a stateless `POST` through the same `wasi:http` transport.
	let mut connection = HttpConnection::new(
		transport,
		origin.upload_pack(),
		UPLOAD_PACK_REQUEST,
		advertisement.to_vec(),
	);
	// The component does not expose shallow clone yet, so it always requests full history. No `insteadOf`
	// rewriting here, so the origin's own persisted URL is the one recorded; and no committer identity
	// through the component's descriptors, so clone writes no reflog.
	gitana_porcelain::clone(
		&mut connection,
		repo,
		work,
		&Deepen::default(),
		None,
		&origin.persisted_url(),
	)
	.await
	.map_err(remote_error)
}

/// The remote composites surface `anyhow::Error` (network, protocol, and storage failures all
/// funnel through it); there is no finer variant to recover, so map to `backend`.
fn remote_error(error: anyhow::Error) -> RepoError {
	RepoError::Backend(format!("{error:#}"))
}
