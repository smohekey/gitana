//! The byte-level transport a stateful git pack negotiation runs over.

use std::future::Future;

use anyhow::Result;

/// A single bidirectional stream to a remote `git-upload-pack` / `git-receive-pack` — the byte-level
/// operations the pkt-line negotiation in [`PackConnection`](crate::PackConnection) needs, abstracted
/// over the transport that carries them. The native SSH transport implements it over an `ssh`
/// subprocess's stdio; the wasm component implements it over a host-granted SSH stream. All the
/// pkt-line framing, ACK-batch parsing, and `multi_ack_detailed` negotiation is shared above this seam —
/// an implementor supplies only the raw I/O and the transport's exit status.
pub trait PackStream {
	/// Write `bytes` to the request side and flush, leaving it open for a later write.
	fn write(&mut self, bytes: &[u8]) -> impl Future<Output = Result<()>>;

	/// Close the request side (the client has nothing more to send), so the server proceeds to its
	/// response. Called at most once per stream.
	fn shutdown_write(&mut self) -> impl Future<Output = Result<()>>;

	/// Read exactly `buf.len()` bytes from the response side (used for pkt-line framing).
	fn read_exact(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<()>>;

	/// Read the rest of the response to EOF (the final ACK and the side-band packfile).
	fn read_to_end(&mut self) -> impl Future<Output = Result<Vec<u8>>>;

	/// Await the transport's completion and fail on an unsuccessful exit — e.g. a nonzero `ssh` status,
	/// which stock git reports as a transport error. Called once, from
	/// [`Connection::finish`](crate::Connection::finish).
	fn await_exit(&mut self) -> impl Future<Output = Result<()>>;
}
