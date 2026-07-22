//! A [`PackFetcher`] over a Smart HTTP transport — the stateless-RPC negotiation.

use anyhow::Result;
use gitana_file_store::FileStore;
use gitana_git_http::Deepen;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

use crate::{HttpTransport, Origin, PackFetcher, fetch_pack};

/// A [`PackFetcher`] that negotiates over Smart HTTP: [`fetch_pack`]'s stateless-RPC loop, which
/// re-POSTs the growing have-set each round. Holds the transport and origin the loop needs.
pub struct HttpPackFetcher<'a, T: HttpTransport> {
	transport: &'a T,
	origin: &'a Origin,
}

impl<'a, T: HttpTransport> HttpPackFetcher<'a, T> {
	/// A fetcher negotiating with `origin` over `transport`.
	pub fn new(transport: &'a T, origin: &'a Origin) -> Self {
		Self { transport, origin }
	}
}

impl<T: HttpTransport> PackFetcher for HttpPackFetcher<'_, T> {
	async fn fetch_pack<F: FileStore, H: HashAlgorithm>(
		&mut self,
		repo: &Repository<F, H>,
		wants: &[ObjectId<H>],
		haves: &[ObjectId<H>],
		deepen: &Deepen,
		include_tag: bool,
	) -> Result<()> {
		fetch_pack(
			self.transport,
			self.origin,
			repo,
			wants,
			haves,
			deepen,
			include_tag,
		)
		.await
	}
}
