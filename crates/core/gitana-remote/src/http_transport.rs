//! The HTTP transport seam for Git Smart HTTP remotes.

use std::future::Future;

use anyhow::Result;

/// A minimal HTTP transport for Git Smart HTTP: `GET` (ref advertisements) and `POST`
/// (`git-upload-pack` / `git-receive-pack`), each returning the **complete** response body.
///
/// Smart HTTP v0 as gitana speaks it is strictly request → whole response — the pack response is
/// parsed in full, with no sideband interleave consumed mid-stream — so a transport need not expose
/// streaming. This keeps the seam small enough that a synchronous, pollable-free implementation
/// (e.g. an in-guest `wasi:http` client on `wasm32-wasip2`) satisfies it alongside the native
/// `ReqwestTransport` (behind the default `reqwest-transport` feature).
pub trait HttpTransport {
	/// `GET` the raw bytes at `url` (a smart-http advertisement endpoint).
	fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>>>;

	/// `POST` `body` with `content_type` to `url`, returning the response bytes.
	fn post(
		&self,
		url: &str,
		content_type: &str,
		body: Vec<u8>,
	) -> impl Future<Output = Result<Vec<u8>>>;
}
