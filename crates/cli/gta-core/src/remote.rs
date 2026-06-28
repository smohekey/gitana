//! HTTP helpers for Git Smart HTTP remotes.

use anyhow::{Context, Result, bail};

/// GET raw bytes from a git smart-http endpoint (advertisements).
pub async fn http_get(url: &str) -> Result<Vec<u8>> {
	let client = reqwest::Client::new();
	let request = client.get(url);
	let response = request.send().await.with_context(|| format!("GET {url}"))?;
	let status = response.status();
	let bytes = response.bytes().await.context("reading response body")?;
	if !status.is_success() {
		bail!("{url}: HTTP {status}: {}", String::from_utf8_lossy(&bytes));
	}
	Ok(bytes.to_vec())
}

/// POST a raw body to a git smart-http endpoint, returning the response bytes.
pub async fn http_post(url: &str, content_type: &str, body: Vec<u8>) -> Result<Vec<u8>> {
	let client = reqwest::Client::new();
	let request = client
		.post(url)
		.header(reqwest::header::CONTENT_TYPE, content_type)
		.body(body);
	let response = request
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
