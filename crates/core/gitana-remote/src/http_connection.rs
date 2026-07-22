//! A [`Connection`] backed by a Smart HTTP [`HttpTransport`].

use anyhow::Result;

use crate::{Connection, HttpTransport};

/// A [`Connection`] over an [`HttpTransport`]: the caller has already fetched the ref advertisement (a
/// `GET .../info/refs?service=…`), and each [`exchange`](Connection::exchange) is a stateless `POST` to
/// the service `endpoint`. This adapts the existing Smart HTTP transport to the connection seam, so
/// `clone` drives HTTP and SSH through one code path.
pub struct HttpConnection<'a, T: HttpTransport> {
	transport: &'a T,
	endpoint: String,
	content_type: &'static str,
	advertisement: Vec<u8>,
}

impl<'a, T: HttpTransport> HttpConnection<'a, T> {
	/// Wrap `transport` as a connection to `endpoint` (e.g. `origin.upload_pack()`), sending
	/// `content_type` on each exchange, over the already-fetched `advertisement` bytes.
	pub fn new(
		transport: &'a T,
		endpoint: String,
		content_type: &'static str,
		advertisement: Vec<u8>,
	) -> Self {
		Self {
			transport,
			endpoint,
			content_type,
			advertisement,
		}
	}
}

impl<T: HttpTransport> Connection for HttpConnection<'_, T> {
	fn advertisement(&self) -> &[u8] {
		&self.advertisement
	}

	async fn exchange(&mut self, body: Vec<u8>) -> Result<Vec<u8>> {
		self
			.transport
			.post(&self.endpoint, self.content_type, body)
			.await
	}

	async fn finish(&mut self) -> Result<()> {
		// Stateless HTTP has no session to finalise — each `exchange` is a self-contained request.
		Ok(())
	}
}
