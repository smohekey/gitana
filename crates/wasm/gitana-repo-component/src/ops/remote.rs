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
	AuthTransport, Connection, HttpConnection, HttpPackFetcher, Origin, PackConnection,
	RECEIVE_PACK_REQUEST, RemoteUrl, SshPackFetcher, SshRemote, UPLOAD_PACK_REQUEST,
};
use gitana_repository::Repository;

use super::WasiCredentialProvider;
use crate::bindings::exports::gitana::repo::porcelain::{
	FetchOutcome, HashKind, PushOutcome as WitPushOutcome, PushSummary, RefEntry, RepoError,
};
use crate::wasi_http_transport::WasiHttpTransport;
use crate::wasi_ssh_transport::WasiSshStream;

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

/// Fetch from the remote at `url` into `repo`, returning the tracking-ref outcome. Dispatches on the
/// URL scheme — Smart HTTP over the `wasi:http` capability, or SSH over the host `ssh-transport`
/// capability. The advertised object-format is checked against `repo`'s before any objects download.
pub(crate) async fn fetch<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	url: &str,
) -> Result<FetchOutcome, RepoError> {
	match parse_remote_url(url)? {
		RemoteUrl::Http(origin) => fetch_http(repo, &origin).await,
		RemoteUrl::Ssh(ssh) => fetch_ssh(repo, &ssh).await,
	}
}

/// Parse a remote URL for a component operation, first rejecting a DOS-drive-prefixed path (`C:/repo`,
/// `C:\repo`). A wasm component cannot know its host's OS, so git's platform gate — which treats
/// `C:/repo` as a *local path* on Windows but as the scp remote `host = C` elsewhere — is unavailable
/// here (`SshRemote`'s `has_dos_drive_prefix` is compiled for the wasm target, never Windows). Rather
/// than dispatch such a path to the SSH provider on a Windows host, the component refuses it as
/// unsupported, matching what the pre-SSH (HTTP-only) component did. This sacrifices the exotic
/// single-character scp hostname, which is not worth an unintended connection from a fat-fingered path.
pub(crate) fn parse_remote_url(url: &str) -> Result<RemoteUrl, RepoError> {
	if looks_like_dos_drive_path(url) {
		return Err(RepoError::Invalid(format!(
			"unsupported remote URL (looks like a local path): {}",
			gitana_remote::anonymize_url(url)
		)));
	}
	RemoteUrl::parse(url).map_err(|e| RepoError::Invalid(e.to_string()))
}

/// Whether `url` begins with a `<letter>:` DOS-drive prefix (git's `has_dos_drive_prefix`) — a local
/// path on Windows, which a platform-agnostic wasm component cannot distinguish from an scp `host:path`.
fn looks_like_dos_drive_path(url: &str) -> bool {
	let bytes = url.as_bytes();
	bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Fetch from the Smart HTTP remote `origin` over the `wasi:http` transport (stateless-RPC negotiation).
async fn fetch_http<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	origin: &Origin,
) -> Result<FetchOutcome, RepoError> {
	let transport = auth_transport(origin);

	let advertisement = gitana_remote::fetch_advertisement(&transport, origin, "git-upload-pack")
		.await
		.map_err(remote_error)?;
	ensure_same_format::<H>(&advertisement)?;

	// The component sees a single worktree through its capability descriptors, so it has no view of any
	// sibling linked worktrees; the current HEAD is still guarded inside the porcelain.
	let mut fetcher = HttpPackFetcher::new(&transport, origin);
	let outcome = run_fetch(&mut fetcher, repo, &advertisement).await?;
	Ok(to_fetch_outcome(outcome))
}

/// Fetch from the SSH remote `ssh`: open a stateful `git-upload-pack` session over the host capability
/// (its ref advertisement arrives on connect), then run git's `multi_ack_detailed` negotiation over it.
async fn fetch_ssh<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	ssh: &SshRemote,
) -> Result<FetchOutcome, RepoError> {
	let connection = open_ssh_connection(ssh, "git-upload-pack").await?;
	let advertisement = connection.advertisement().to_vec();
	ensure_same_format::<H>(&advertisement)?;

	let mut fetcher = SshPackFetcher::new(connection);
	let outcome = run_fetch(&mut fetcher, repo, &advertisement).await?;
	Ok(to_fetch_outcome(outcome))
}

/// Run the porcelain fetch over `fetcher` with the component's fixed policy (auto-follow tags, no
/// shallow, no reflog — the component resolves no committer identity through its descriptors). Shared by
/// the HTTP and SSH paths, which differ only in the `PackFetcher`.
async fn run_fetch<H: HashAlgorithm>(
	fetcher: &mut impl gitana_remote::PackFetcher,
	repo: &Repository<WorktreeFileStore, H>,
	advertisement: &[u8],
) -> Result<gitana_porcelain::FetchOutcome<H>, RepoError> {
	gitana_porcelain::fetch(
		fetcher,
		repo,
		advertisement,
		false,
		gitana_porcelain::TagFetch::Auto,
		&gitana_porcelain::Deepen::default(),
		&[],
		None,
	)
	.await
	.map_err(remote_error)
}

/// Map the porcelain fetch outcome into the WIT record.
fn to_fetch_outcome<H: HashAlgorithm>(outcome: gitana_porcelain::FetchOutcome<H>) -> FetchOutcome {
	FetchOutcome {
		updated: outcome
			.updated
			.into_iter()
			.map(|(name, id)| RefEntry {
				name,
				id: id.to_hex(),
			})
			.collect(),
		rejected: outcome.rejected,
	}
}

/// Push `HEAD`'s branch to the remote at `url` (or, with `delete`, remove a remote branch),
/// returning the outcome. Dispatches on the URL scheme — Smart HTTP or SSH. The advertised
/// object-format is checked against `repo`'s first. This is an unsigned push: certificate signing shells
/// out to `ssh-keygen`, which the component has no authority to do, so `gta push --signed` stays on the CLI.
pub(crate) async fn push<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	url: &str,
	force: bool,
	delete: Option<String>,
) -> Result<WitPushOutcome, RepoError> {
	match parse_remote_url(url)? {
		RemoteUrl::Http(origin) => push_http(repo, &origin, force, delete).await,
		RemoteUrl::Ssh(ssh) => push_ssh(repo, &ssh, force, delete).await,
	}
}

/// Push to the Smart HTTP remote `origin`: one receive-pack exchange over the `wasi:http` transport
/// (the advertisement is fetched separately and passed to the porcelain, so the connection carries none).
async fn push_http<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	origin: &Origin,
	force: bool,
	delete: Option<String>,
) -> Result<WitPushOutcome, RepoError> {
	let transport = auth_transport(origin);
	let advertisement = gitana_remote::fetch_advertisement(&transport, origin, "git-receive-pack")
		.await
		.map_err(remote_error)?;
	ensure_same_format::<H>(&advertisement)?;

	let mut connection = HttpConnection::new(
		&transport,
		origin.receive_pack(),
		RECEIVE_PACK_REQUEST,
		Vec::new(),
	);
	let outcome = run_push(&mut connection, repo, &advertisement, force, delete).await?;
	Ok(to_push_outcome(outcome))
}

/// Push to the SSH remote `ssh`: open a `git-receive-pack` session over the host capability (its ref
/// advertisement arrives on connect) and send the update in a single exchange over that connection.
async fn push_ssh<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	ssh: &SshRemote,
	force: bool,
	delete: Option<String>,
) -> Result<WitPushOutcome, RepoError> {
	let mut connection = open_ssh_connection(ssh, "git-receive-pack").await?;
	let advertisement = connection.advertisement().to_vec();
	ensure_same_format::<H>(&advertisement)?;

	let outcome = run_push(&mut connection, repo, &advertisement, force, delete).await?;
	Ok(to_push_outcome(outcome))
}

/// Run the porcelain push of `HEAD`'s branch (or a single-ref delete) over `connection`. Shared by the
/// HTTP and SSH paths, which differ only in the `Connection`.
async fn run_push<H: HashAlgorithm>(
	connection: &mut impl gitana_remote::Connection,
	repo: &Repository<WorktreeFileStore, H>,
	advertisement: &[u8],
	force: bool,
	delete: Option<String>,
) -> Result<gitana_porcelain::PushOutcome, RepoError> {
	// The component surface pushes `HEAD`'s branch or deletes one ref — an explicit refspec (not an empty
	// list): the porcelain default would honour `remote.origin.push`, which could push a different or
	// multiple refs and break this WIT contract of "push HEAD's branch" (and the single-result mapping).
	let refspecs = match delete {
		Some(target) => vec![
			gitana_remote::PushRefspec::parse(&format!(":{target}"))
				.map_err(|e| RepoError::Invalid(e.to_string()))?,
		],
		None => vec![
			gitana_remote::PushRefspec::parse("HEAD").map_err(|e| RepoError::Invalid(e.to_string()))?,
		],
	};
	// The component surface pushes a single ref, so atomicity would be a no-op; it is not exposed in the
	// WIT contract.
	gitana_porcelain::push(
		connection,
		repo,
		advertisement,
		force,
		false,
		refspecs,
		gitana_porcelain::PushTags::None,
	)
	.await
	.map_err(remote_error)
}

/// Map the porcelain push outcome into the WIT variant. Exactly one result in the component case (a
/// branch push or a single delete), or none when the remote was already up to date.
fn to_push_outcome(outcome: gitana_porcelain::PushOutcome) -> WitPushOutcome {
	match outcome.results.first() {
		None => WitPushOutcome::UpToDate,
		Some(result) if result.deleted => WitPushOutcome::Deleted(result.refname.clone()),
		Some(result) => WitPushOutcome::Pushed(PushSummary {
			branch: result.refname.clone(),
			forced: result.forced,
		}),
	}
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

/// Require the remote's advertised object-format to match the local repository's `H` — objects of one
/// hash cannot be stored in a repository of the other.
fn ensure_same_format<H: HashAlgorithm>(advertisement: &[u8]) -> Result<(), RepoError> {
	let remote = gitana_remote::negotiated_kind(advertisement).map_err(remote_error)?;
	if H::NAME != remote.name() {
		return Err(RepoError::Invalid(format!(
			"remote object-format is {}, but the local repository is {}",
			remote.name(),
			H::NAME
		)));
	}
	Ok(())
}

/// Open a stateful SSH connection to `service` (`git-upload-pack` / `git-receive-pack`) on `ssh` over
/// the host `ssh-transport` capability, reading the ref advertisement the server sends on connect.
async fn open_ssh_connection(
	ssh: &SshRemote,
	service: &str,
) -> Result<PackConnection<WasiSshStream>, RepoError> {
	let stream = WasiSshStream::open(service, ssh).map_err(remote_error)?;
	PackConnection::open_over(stream)
		.await
		.map_err(remote_error)
}

/// Open a `git-upload-pack` SSH session for a clone and read the object format the remote advertises —
/// the pre-dispatch step run before any local repository exists (the SSH counterpart of
/// [`clone_negotiate`]). Returns the opened connection, its advertisement already read, so the caller
/// drives the clone over it under the matching `H`, plus the negotiated [`HashKind`].
pub(crate) async fn open_ssh_clone(
	ssh: &SshRemote,
) -> Result<(PackConnection<WasiSshStream>, HashKind), RepoError> {
	let connection = open_ssh_connection(ssh, "git-upload-pack").await?;
	let kind =
		match gitana_remote::negotiated_kind(connection.advertisement()).map_err(remote_error)? {
			gitana_object::HashKind::Sha1 => HashKind::Sha1,
			gitana_object::HashKind::Sha256 => HashKind::Sha256,
		};
	Ok((connection, kind))
}

/// Clone the SSH remote into a fresh checkout backed by `store` (the git directory) and `work` (the
/// working tree) as hash `H`, driving `gitana-porcelain`'s clone over the already-opened `connection`
/// (its advertisement read by [`open_ssh_clone`]). `url` is the original clone argument, persisted as
/// `remote.origin.url` with any password redacted — matching the CLI. There is no `insteadOf` rewriting
/// here, and no committer identity through the component's descriptors, so clone writes no reflog.
pub(crate) async fn clone_ssh<H: HashAlgorithm>(
	mut connection: PackConnection<WasiSshStream>,
	store: WorktreeFileStore,
	work: DescriptorWorkDir,
	url: &str,
) -> Result<(), RepoError> {
	let repo: Repository<WorktreeFileStore, H> = Repository::new(ObjectStore::new(store));
	gitana_porcelain::clone(
		&mut connection,
		repo,
		work,
		&Deepen::default(),
		None,
		&gitana_remote::redact_password(url),
	)
	.await
	.map_err(remote_error)
}

/// The remote composites surface `anyhow::Error` (network, protocol, and storage failures all
/// funnel through it); there is no finer variant to recover, so map to `backend`.
fn remote_error(error: anyhow::Error) -> RepoError {
	RepoError::Backend(format!("{error:#}"))
}
