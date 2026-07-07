//! Remote operations — `fetch`, `push`, and `clone` over the in-guest `wasi:http` transport.
//!
//! These reuse `gitana-porcelain`'s remote composites unchanged, injecting the component's
//! [`WasiHttpTransport`] as the `HttpTransport` capability. The advertisement GET and the pack POST
//! both flow through `wasi:http`; nothing here reaches for `reqwest`.

use gitana_file_store_local::{DescriptorWorkDir, WorktreeFileStore};
use gitana_object::HashAlgorithm;
use gitana_object_store::ObjectStore;
use gitana_remote::Origin;
use gitana_repository::Repository;

use crate::bindings::exports::gitana::repo::porcelain::{
	FetchOutcome, HashKind, PushOutcome as WitPushOutcome, PushSummary, RefEntry, RepoError,
};
use crate::wasi_http_transport::WasiHttpTransport;

/// Fetch from the remote at `url` into `repo`, returning the tracking-ref outcome. The advertised
/// object-format is checked against `repo`'s before any objects are downloaded.
pub(crate) async fn fetch<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	url: &str,
) -> Result<FetchOutcome, RepoError> {
	let origin = Origin::parse(url).map_err(|e| RepoError::Invalid(e.to_string()))?;
	let transport = WasiHttpTransport;

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

	let outcome = gitana_porcelain::fetch(&transport, repo, &origin, &advertisement, false)
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
	let transport = WasiHttpTransport;

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
	let outcome = gitana_porcelain::push(&transport, repo, &origin, &advertisement, force, refspecs)
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
/// creates the repository as `HashKind` and hands the advertisement to [`clone`] (no second GET).
pub(crate) async fn clone_negotiate(origin: &Origin) -> Result<(Vec<u8>, HashKind), RepoError> {
	let transport = WasiHttpTransport;
	let advertisement = gitana_remote::fetch_advertisement(&transport, origin, "git-upload-pack")
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
/// (init, download, recreate refs/HEAD, save origin, check out `HEAD`) unchanged.
pub(crate) async fn clone<H: HashAlgorithm>(
	store: WorktreeFileStore,
	work: DescriptorWorkDir,
	origin: &Origin,
	advertisement: &[u8],
) -> Result<(), RepoError> {
	let transport = WasiHttpTransport;
	let repo: Repository<WorktreeFileStore, H> = Repository::new(ObjectStore::new(store));
	gitana_porcelain::clone(&transport, repo, origin, advertisement, work)
		.await
		.map_err(remote_error)
}

/// The remote composites surface `anyhow::Error` (network, protocol, and storage failures all
/// funnel through it); there is no finer variant to recover, so map to `backend`.
fn remote_error(error: anyhow::Error) -> RepoError {
	RepoError::Backend(format!("{error:#}"))
}
