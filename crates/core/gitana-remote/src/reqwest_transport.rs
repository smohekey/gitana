//! The native, `reqwest`-backed [`HttpClient`]. Behind the default `reqwest-transport` feature so a
//! wasm build can omit `reqwest` entirely and supply its own client.

use anyhow::{Context, Result};

use crate::{HttpClient, HttpResponse};

/// An [`HttpClient`] over a pooled `reqwest::Client`, optionally attaching a fixed set of extra request
/// headers to every request (git's `http.extraHeader`).
#[derive(Debug, Clone, Default)]
pub struct ReqwestTransport {
	client: reqwest::Client,
	/// Headers added to every request, ahead of any the caller passes (e.g. `Authorization`) — git's
	/// configured `http.extraHeader` values. Empty for a plain transport.
	extra_headers: Vec<(String, String)>,
}

impl ReqwestTransport {
	/// A transport with a fresh client (its own connection pool) and no extra headers.
	pub fn new() -> Self {
		Self::default()
	}

	/// A transport that attaches `extra_headers` (git's `http.extraHeader`) to every request it issues,
	/// ahead of any per-request headers the auth layer adds.
	pub fn with_extra_headers(extra_headers: Vec<(String, String)>) -> Self {
		Self {
			client: reqwest::Client::new(),
			extra_headers,
		}
	}
}

/// Read a finished reqwest response into an [`HttpResponse`] (status + whole body). A non-2xx status
/// is **not** an error here — the credential layer above decides what a `401` means; only a genuine
/// transport failure (which already returned `Err`) stops a request short.
async fn read_response(response: reqwest::Response) -> Result<HttpResponse> {
	let status = response.status().as_u16();
	// A 401 may carry the challenge across several `WWW-Authenticate` fields (e.g. `Negotiate` then
	// `Basic`); keep each field as a distinct value so a Basic offer in any is seen and each is forwarded
	// to a credential helper as its own `wwwauth[]` line, as git does.
	let www_authenticate: Vec<String> = response
		.headers()
		.get_all(reqwest::header::WWW_AUTHENTICATE)
		.iter()
		.filter_map(|value| value.to_str().ok())
		.map(str::to_owned)
		.collect();
	let body = response.bytes().await.context("reading response body")?;
	Ok(HttpResponse {
		status,
		www_authenticate,
		body: body.to_vec(),
	})
}

/// Apply `(name, value)` request headers to a reqwest builder.
fn with_headers(
	mut builder: reqwest::RequestBuilder,
	headers: &[(String, String)],
) -> reqwest::RequestBuilder {
	for (name, value) in headers {
		builder = builder.header(name, value);
	}
	builder
}

impl HttpClient for ReqwestTransport {
	async fn get(&self, url: &str, headers: &[(String, String)]) -> Result<HttpResponse> {
		let builder = with_headers(self.client.get(url), &self.extra_headers);
		let response = with_headers(builder, headers)
			.send()
			.await
			.with_context(|| format!("GET {url}"))?;
		read_response(response).await
	}

	async fn post(
		&self,
		url: &str,
		content_type: &str,
		body: Vec<u8>,
		headers: &[(String, String)],
	) -> Result<HttpResponse> {
		let builder = self
			.client
			.post(url)
			.header(reqwest::header::CONTENT_TYPE, content_type)
			.body(body);
		let builder = with_headers(builder, &self.extra_headers);
		let response = with_headers(builder, headers)
			.send()
			.await
			.with_context(|| format!("POST {url}"))?;
		read_response(response).await
	}
}
