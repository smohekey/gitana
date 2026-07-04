//! In-guest `wasi:http` transport for the Smart HTTP remote porcelain.
//!
//! Implements [`gitana_remote::HttpTransport`] over the host-granted
//! `wasi:http/outgoing-handler` capability. Every method is synchronous under the hood — it blocks
//! on `wasi:io` pollables inline ([`Pollable::block`], [`InputStream::blocking_read`],
//! [`OutputStream::blocking_write_and_flush`]) and returns an already-`Ready` future — so the
//! component's noop-waker [`block_on`](crate::block_on) drives it without ever seeing `Pending`,
//! exactly as the descriptor file store does. That keeps the WASI 0.2 sync-export invariant intact:
//! no future parked on a pollable ever reaches the executor.
//!
//! Smart HTTP v0 is request → whole response, so the response body is read to completion here; no
//! sideband is streamed. Request bodies are written in ≤4 KiB chunks because WASI's
//! `blocking-write-and-flush` accepts no more per call.

use anyhow::{Result, anyhow, bail};
use gitana_remote::HttpTransport;

use wasip2::http::outgoing_handler;
use wasip2::http::types::{
	Fields, IncomingResponse, Method, OutgoingBody, OutgoingRequest, Scheme,
};
use wasip2::io::streams::StreamError;

/// The most bytes a single `blocking-write-and-flush` may accept (WASI 0.2 caps it at 4096).
const WRITE_CHUNK: usize = 4096;
/// Response-body read granularity.
const READ_CHUNK: u64 = 64 * 1024;

/// An [`HttpTransport`] over the component's `wasi:http` outgoing-handler capability.
pub(crate) struct WasiHttpTransport;

impl HttpTransport for WasiHttpTransport {
	async fn get(&self, url: &str) -> Result<Vec<u8>> {
		request(Method::Get, url, None, &[])
	}

	async fn post(&self, url: &str, content_type: &str, body: Vec<u8>) -> Result<Vec<u8>> {
		request(Method::Post, url, Some(content_type), &body)
	}
}

/// Issue one request and read the whole response body, failing on a non-2xx status. Fully
/// synchronous: the only waits are inline `Pollable::block` / blocking stream I/O.
fn request(method: Method, url: &str, content_type: Option<&str>, body: &[u8]) -> Result<Vec<u8>> {
	let (scheme, authority, path_with_query) = split_url(url)?;

	let headers = match content_type {
		Some(ct) => Fields::from_list(&[("content-type".to_owned(), ct.as_bytes().to_vec())])
			.map_err(|e| anyhow!("building request headers: {e:?}"))?,
		None => Fields::new(),
	};

	let request = OutgoingRequest::new(headers);
	request
		.set_method(&method)
		.map_err(|()| anyhow!("rejected request method"))?;
	request
		.set_scheme(Some(&scheme))
		.map_err(|()| anyhow!("rejected request scheme"))?;
	request
		.set_authority(Some(&authority))
		.map_err(|()| anyhow!("rejected request authority {authority}"))?;
	request
		.set_path_with_query(Some(&path_with_query))
		.map_err(|()| anyhow!("rejected request path {path_with_query}"))?;

	// Take the body handle before `handle` consumes the request.
	let outgoing_body = request
		.body()
		.map_err(|()| anyhow!("outgoing request body already taken"))?;

	let future = outgoing_handler::handle(request, None)
		.map_err(|e| anyhow!("{url}: wasi:http handle failed: {e:?}"))?;

	write_body(&outgoing_body, body)?;
	// The output stream borrowed by `write_body` is dropped; the body can now be finished.
	OutgoingBody::finish(outgoing_body, None)
		.map_err(|e| anyhow!("finishing request body: {e:?}"))?;

	// Synchronously wait for the response — this is the one blocking point, and it never yields
	// `Pending` to our executor.
	let pollable = future.subscribe();
	pollable.block();
	let response = future
		.get()
		.ok_or_else(|| anyhow!("{url}: response future ready but empty"))?
		.map_err(|()| anyhow!("{url}: response already taken"))?
		.map_err(|e| anyhow!("{url}: request failed: {e:?}"))?;

	let status = response.status();
	let body = read_body(&response)?;
	if !(200..300).contains(&status) {
		bail!("{url}: HTTP {status}: {}", String::from_utf8_lossy(&body));
	}
	Ok(body)
}

/// Write `bytes` to the request's outgoing body in ≤[`WRITE_CHUNK`] slices (empty = no write).
fn write_body(body: &OutgoingBody, bytes: &[u8]) -> Result<()> {
	if bytes.is_empty() {
		return Ok(());
	}
	let stream = body
		.write()
		.map_err(|()| anyhow!("outgoing body stream unavailable"))?;
	for chunk in bytes.chunks(WRITE_CHUNK) {
		stream
			.blocking_write_and_flush(chunk)
			.map_err(|e| anyhow!("writing request body: {e:?}"))?;
	}
	Ok(())
}

/// Read the response body to completion, blocking until the stream closes.
fn read_body(response: &IncomingResponse) -> Result<Vec<u8>> {
	let body = response
		.consume()
		.map_err(|()| anyhow!("response body already consumed"))?;
	let stream = body
		.stream()
		.map_err(|()| anyhow!("response body stream unavailable"))?;
	let mut out = Vec::new();
	loop {
		match stream.blocking_read(READ_CHUNK) {
			Ok(chunk) => out.extend_from_slice(&chunk),
			Err(StreamError::Closed) => break,
			Err(StreamError::LastOperationFailed(e)) => {
				bail!("reading response body: {}", e.to_debug_string());
			}
		}
	}
	Ok(out)
}

/// Split a Smart HTTP URL into its `wasi:http` request components: scheme, authority (`host[:port]`),
/// and path-with-query. Only `http`/`https` are accepted.
fn split_url(url: &str) -> Result<(Scheme, String, String)> {
	let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
		(Scheme::Https, rest)
	} else if let Some(rest) = url.strip_prefix("http://") {
		(Scheme::Http, rest)
	} else {
		bail!("unsupported URL scheme (expected http/https): {url}");
	};
	let (authority, path_with_query) = match rest.find('/') {
		Some(i) => (&rest[..i], &rest[i..]),
		None => (rest, "/"),
	};
	if authority.is_empty() {
		bail!("URL has no host: {url}");
	}
	Ok((scheme, authority.to_owned(), path_with_query.to_owned()))
}
