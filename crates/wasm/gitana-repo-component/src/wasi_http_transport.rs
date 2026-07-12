//! In-guest `wasi:http` client for the Smart HTTP remote porcelain.
//!
//! Implements [`gitana_remote::HttpClient`] over the host-granted `wasi:http/outgoing-handler`
//! capability. Every method is synchronous under the hood — it blocks on `wasi:io` pollables inline
//! ([`Pollable::block`], [`InputStream::blocking_read`], [`OutputStream::blocking_write_and_flush`])
//! and returns an already-`Ready` future — so the component's noop-waker
//! [`block_on`](crate::block_on) drives it without ever seeing `Pending`, exactly as the descriptor
//! file store does. That keeps the WASI 0.2 sync-export invariant intact: no future parked on a
//! pollable ever reaches the executor.
//!
//! As a raw [`HttpClient`], it reports the response **status** rather than turning a non-2xx into an
//! error and forwards the caller's request headers verbatim (e.g. an `Authorization`) — the
//! credential-aware wrapping is done above it, in `gitana-remote`.
//!
//! Smart HTTP v0 is request → whole response, so the response body is read to completion here; no
//! sideband is streamed. Request bodies are written in ≤4 KiB chunks because WASI's
//! `blocking-write-and-flush` accepts no more per call.

use anyhow::{Result, anyhow, bail};
use gitana_remote::{HttpClient, HttpResponse};

use wasip2::http::outgoing_handler;
use wasip2::http::types::{
	Fields, IncomingResponse, Method, OutgoingBody, OutgoingRequest, Scheme,
};
use wasip2::io::streams::StreamError;

/// The most bytes a single `blocking-write-and-flush` may accept (WASI 0.2 caps it at 4096).
const WRITE_CHUNK: usize = 4096;
/// Response-body read granularity.
const READ_CHUNK: u64 = 64 * 1024;

/// An [`HttpClient`] over the component's `wasi:http` outgoing-handler capability.
pub(crate) struct WasiHttpTransport;

impl HttpClient for WasiHttpTransport {
	async fn get(&self, url: &str, headers: &[(String, String)]) -> Result<HttpResponse> {
		request(Method::Get, url, None, &[], headers)
	}

	async fn post(
		&self,
		url: &str,
		content_type: &str,
		body: Vec<u8>,
		headers: &[(String, String)],
	) -> Result<HttpResponse> {
		request(Method::Post, url, Some(content_type), &body, headers)
	}
}

/// Issue one request and read the whole response body, returning its status and bytes (no non-2xx
/// handling — the credential layer above decides). Fully synchronous: the only waits are inline
/// `Pollable::block` / blocking stream I/O.
fn request(
	method: Method,
	url: &str,
	content_type: Option<&str>,
	body: &[u8],
	extra_headers: &[(String, String)],
) -> Result<HttpResponse> {
	let (scheme, authority, path_with_query) = split_url(url)?;

	// content-type (POST) plus any caller-supplied request headers (e.g. `Authorization`).
	let mut header_list: Vec<(String, Vec<u8>)> = Vec::new();
	if let Some(ct) = content_type {
		header_list.push(("content-type".to_owned(), ct.as_bytes().to_vec()));
	}
	for (name, value) in extra_headers {
		header_list.push((name.clone(), value.as_bytes().to_vec()));
	}
	let headers =
		Fields::from_list(&header_list).map_err(|e| anyhow!("building request headers: {e:?}"))?;

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
	let www_authenticate = www_authenticate(&response);
	let body = read_body(&response)?;
	Ok(HttpResponse {
		status,
		www_authenticate,
		body,
	})
}

/// The response's `WWW-Authenticate` challenge, if present — surfaced so the credential layer only
/// offers Basic when the server asked for it. All matching header fields (case-insensitive name,
/// UTF-8 decodable) are joined, since a 401 may split its schemes across several fields.
fn www_authenticate(response: &IncomingResponse) -> Option<String> {
	let challenges: Vec<String> = response
		.headers()
		.entries()
		.into_iter()
		.filter(|(name, _)| name.eq_ignore_ascii_case("www-authenticate"))
		.filter_map(|(_, value)| String::from_utf8(value).ok())
		.collect();
	(!challenges.is_empty()).then(|| challenges.join(", "))
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
