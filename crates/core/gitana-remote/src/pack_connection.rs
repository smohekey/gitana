//! A [`Connection`] over any [`PackStream`] — the shared pkt-line negotiation, transport-independent.

use anyhow::{Context, Result, bail};

use crate::{Connection, PackStream};

/// A git pack connection over a [`PackStream`]: the ref advertisement (read when the stream opened) plus
/// the pkt-line request/response machinery both Smart HTTP-style single exchanges ([`Connection`]) and
/// the SSH stateful fetch negotiation use. Generic over the byte transport `S`, so the native `ssh`
/// subprocess and the wasm host-granted SSH stream share this logic — they differ only in `PackStream`.
pub struct PackConnection<S: PackStream> {
	stream: S,
	advertisement: Vec<u8>,
	/// Whether a request was written, so [`finish`](Connection::finish) knows whether it still owes the
	/// terminating flush an empty clone never sent.
	request_sent: bool,
}

impl<S: PackStream> PackConnection<S> {
	/// Wrap an opened `stream`, reading the ref advertisement the server sends on connect (SSH omits the
	/// `# service=` banner Smart HTTP prepends, which [`parse_advertisement`](gitana_git_http::parse_advertisement)
	/// already tolerates).
	pub async fn open_over(mut stream: S) -> Result<Self> {
		let advertisement = read_advertisement(&mut stream)
			.await
			.context("reading the ssh ref advertisement")?;
		Ok(Self {
			stream,
			advertisement,
			request_sent: false,
		})
	}

	/// Write one negotiation message (pkt-lines) without closing the request side — for a multi-round
	/// fetch, where the stream stays open between rounds and is closed only in
	/// [`read_pack`](Self::read_pack) after `done`.
	pub(crate) async fn write(&mut self, bytes: &[u8]) -> Result<()> {
		self.stream.write(bytes).await?;
		self.request_sent = true;
		Ok(())
	}

	/// Read one acknowledgment batch (the server's response to a have-group) and report whether the
	/// client should now send `done`. The boundary depends on the negotiation mode the server chose:
	/// - `multi_ack_detailed`: `ACK <oid> common`* then optionally `ACK <oid> ready`, terminated by `NAK`.
	/// - plain `multi_ack`: `ACK <oid> continue`* terminated by `NAK` (no `ready` — keep offering haves).
	/// - single-ack (base v0): a bare `ACK <oid>` is terminal — the server then stays silent until `done`.
	///
	/// (`common`/`continue` acks are consumed but not yet used to prune later have-groups — a follow-up.)
	pub(crate) async fn read_ack_batch(&mut self) -> Result<bool> {
		let mut ready = false;
		loop {
			match self.read_pkt_line().await? {
				// A flush also ends a batch (defensive).
				None => return Ok(ready),
				Some(line) => {
					if line == b"NAK\n" {
						return Ok(ready);
					}
					if let Some(rest) = line.strip_prefix(b"ACK ") {
						match ack_class(rest.strip_suffix(b"\n").unwrap_or(rest)) {
							AckClass::Ready => ready = true,
							AckClass::BareTerminal => return Ok(true),
							AckClass::Partial => {}
						}
					}
				}
			}
		}
	}

	/// Close the request side and read the final response — the last `ACK` and the side-band packfile —
	/// to EOF, for [`parse_upload_pack_response`](gitana_git_http::parse_upload_pack_response).
	pub(crate) async fn read_pack(&mut self) -> Result<Vec<u8>> {
		self.stream.shutdown_write().await?;
		self.stream.read_to_end().await
	}

	/// Read one pkt-line's payload from the stream, or `None` on a flush-pkt (`0000`).
	async fn read_pkt_line(&mut self) -> Result<Option<Vec<u8>>> {
		let mut len_bytes = [0u8; 4];
		self
			.stream
			.read_exact(&mut len_bytes)
			.await
			.context("reading a pkt-line length")?;
		let len = usize::from_str_radix(
			std::str::from_utf8(&len_bytes).context("pkt-line length is not UTF-8")?,
			16,
		)
		.context("pkt-line length is not hex")?;
		if len == 0 {
			return Ok(None);
		}
		if len < 4 {
			bail!("invalid pkt-line length {len}");
		}
		let mut body = vec![0u8; len - 4];
		self
			.stream
			.read_exact(&mut body)
			.await
			.context("reading a pkt-line body")?;
		Ok(Some(body))
	}
}

impl<S: PackStream> Connection for PackConnection<S> {
	fn advertisement(&self) -> &[u8] {
		&self.advertisement
	}

	async fn exchange(&mut self, body: Vec<u8>) -> Result<Vec<u8>> {
		// A single-round exchange (clone / push): write the request, close the request side, and read the
		// whole response to EOF. The exit status is checked in `finish`.
		self.stream.write(&body).await?;
		self.request_sent = true;
		self.stream.shutdown_write().await?;
		self.stream.read_to_end().await
	}

	async fn finish(&mut self) -> Result<()> {
		// An empty clone requests nothing, so upload-pack is still waiting on the client — send the
		// terminating flush-pkt (`0000`) git owes it and close the request side, so it exits cleanly
		// instead of logging "the remote end hung up unexpectedly".
		if !self.request_sent {
			self.stream.write(b"0000").await?;
			self.stream.shutdown_write().await?;
		}
		self.stream.await_exit().await
	}
}

/// Read the v0 ref advertisement: successive pkt-lines up to and including the terminating flush-pkt
/// (`0000`), returning the raw bytes (banner-free, as SSH sends them) for `parse_advertisement`.
async fn read_advertisement(stream: &mut impl PackStream) -> Result<Vec<u8>> {
	let mut out = Vec::new();
	loop {
		let mut len_bytes = [0u8; 4];
		stream
			.read_exact(&mut len_bytes)
			.await
			.context("reading a pkt-line length")?;
		out.extend_from_slice(&len_bytes);
		let len = usize::from_str_radix(
			std::str::from_utf8(&len_bytes).context("pkt-line length is not UTF-8")?,
			16,
		)
		.context("pkt-line length is not hex")?;
		if len == 0 {
			break;
		}
		if len < 4 {
			bail!("invalid pkt-line length {len}");
		}
		let mut body = vec![0u8; len - 4];
		stream
			.read_exact(&mut body)
			.await
			.context("reading a pkt-line body")?;
		out.extend_from_slice(&body);
	}
	Ok(out)
}

/// The negotiation meaning of an `ACK …` line's tail (the text after `ACK `, trailing newline trimmed).
enum AckClass {
	/// `ACK <oid> ready` — the server has a sufficient cut point (the `NAK` still ends the batch).
	Ready,
	/// `ACK <oid> common` / `ACK <oid> continue` — a common commit under multi_ack; keep reading.
	Partial,
	/// A bare `ACK <oid>` — single-ack base protocol: terminal, since no `NAK` follows.
	BareTerminal,
}

/// Classify an `ACK …` line's tail, distinguishing the multi_ack forms from a bare single-ack `ACK`.
fn ack_class(body: &[u8]) -> AckClass {
	if body.ends_with(b" ready") {
		AckClass::Ready
	} else if body.contains(&b' ') {
		AckClass::Partial
	} else {
		AckClass::BareTerminal
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn classifies_ack_lines_by_mode() {
		assert!(matches!(ack_class(b"abc123 ready"), AckClass::Ready));
		assert!(matches!(ack_class(b"abc123 common"), AckClass::Partial));
		assert!(matches!(ack_class(b"abc123 continue"), AckClass::Partial));
		assert!(matches!(ack_class(b"abc123"), AckClass::BareTerminal));
	}
}
