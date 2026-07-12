//! An [`HttpTransport`] that never authenticates.

use anyhow::Result;

use crate::{HttpClient, HttpTransport};

/// Wraps an [`HttpClient`] as the body-returning [`HttpTransport`] the porcelain consumes, sending no
/// `Authorization` and treating any non-2xx status as an error — exactly the behaviour every caller
/// had before credentials existed. It is what anonymous callers and the wasm component use, so those
/// paths are unchanged, and what pairs a raw client with the porcelain when there is no credential
/// capability to offer.
pub struct Unauthenticated<C> {
	client: C,
}

impl<C: HttpClient> Unauthenticated<C> {
	/// Wrap `client`.
	pub fn new(client: C) -> Self {
		Self { client }
	}
}

impl<C: HttpClient> HttpTransport for Unauthenticated<C> {
	async fn get(&self, url: &str) -> Result<Vec<u8>> {
		self.client.get(url, &[]).await?.into_body(url)
	}

	async fn post(&self, url: &str, content_type: &str, body: Vec<u8>) -> Result<Vec<u8>> {
		self
			.client
			.post(url, content_type, body, &[])
			.await?
			.into_body(url)
	}
}
