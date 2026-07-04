//! The native, `reqwest`-backed [`HttpTransport`]. Behind the default `reqwest-transport` feature so
//! a wasm build can omit `reqwest` entirely and supply its own transport.

use anyhow::{Context, Result, bail};

use crate::HttpTransport;

/// An [`HttpTransport`] over a pooled `reqwest::Client`.
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

impl HttpTransport for ReqwestTransport {
	async fn get(&self, url: &str) -> Result<Vec<u8>> {
		let response = self
			.client
			.get(url)
			.send()
			.await
			.with_context(|| format!("GET {url}"))?;
		let status = response.status();
		let bytes = response.bytes().await.context("reading response body")?;
		if !status.is_success() {
			bail!("{url}: HTTP {status}: {}", String::from_utf8_lossy(&bytes));
		}
		Ok(bytes.to_vec())
	}

	async fn post(&self, url: &str, content_type: &str, body: Vec<u8>) -> Result<Vec<u8>> {
		let response = self
			.client
			.post(url)
			.header(reqwest::header::CONTENT_TYPE, content_type)
			.body(body)
			.send()
			.await
			.with_context(|| format!("POST {url}"))?;
		let status = response.status();
		let bytes = response.bytes().await.context("reading response body")?;
		if !status.is_success() {
			bail!("{url}: HTTP {status}: {}", String::from_utf8_lossy(&bytes));
		}
		Ok(bytes.to_vec())
	}
}
