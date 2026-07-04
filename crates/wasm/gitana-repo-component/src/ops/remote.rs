//! Remote operations — `fetch` over the in-guest `wasi:http` transport.
//!
//! These reuse `gitana-porcelain`'s remote composites unchanged, injecting the component's
//! [`WasiHttpTransport`] as the `HttpTransport` capability. The advertisement GET and the pack POST
//! both flow through `wasi:http`; nothing here reaches for `reqwest`.

use gitana_file_store_local::WorktreeFileStore;
use gitana_object::HashAlgorithm;
use gitana_remote::Origin;
use gitana_repository::Repository;

use crate::bindings::exports::gitana::repo::porcelain::{FetchOutcome, RefEntry, RepoError};
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

/// The remote composites surface `anyhow::Error` (network, protocol, and storage failures all
/// funnel through it); there is no finer variant to recover, so map to `backend`.
fn remote_error(error: anyhow::Error) -> RepoError {
	RepoError::Backend(format!("{error:#}"))
}
