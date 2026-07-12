//! The native, `reqwest`-backed [`HttpClient`]. Behind the default `reqwest-transport` feature so a
//! wasm build can omit `reqwest` entirely and supply its own client.

use anyhow::{Context, Result};

use crate::{HttpClient, HttpResponse};

/// An [`HttpClient`] over a pooled `reqwest::Client`.
#[derive(Debug, Clone, Default)]
pub struct ReqwestTransport {
	client: reqwest::Client,
}

impl ReqwestTransport {
	/// A transport with a fresh client (its own connection pool).
	pub fn new() -> Self {
		Self::default()
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
		let response = with_headers(self.client.get(url), headers)
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
		let response = with_headers(builder, headers)
			.send()
			.await
			.with_context(|| format!("POST {url}"))?;
		read_response(response).await
	}
}
