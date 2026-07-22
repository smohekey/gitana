//! The transport seam shared by Smart HTTP and SSH remotes.

use std::future::Future;

use anyhow::Result;

/// A single session to a git service (`git-upload-pack` / `git-receive-pack`) over some transport.
///
/// Where [`HttpTransport`](crate::HttpTransport) exposes stateless `get`/`post`, a `Connection` models
/// the one shape SSH and Smart HTTP share: the server opens by sending a ref *advertisement*, then
/// answers one or more request bodies. HTTP satisfies it statelessly — the advertisement is a `GET`
/// and each [`exchange`](Connection::exchange) a `POST` (see [`HttpConnection`](crate::HttpConnection));
/// SSH satisfies it over a single bidirectional stream, where the advertisement and every exchange ride
/// the same channel. The pkt-line codec in `gitana-git-http` sits on top of either.
pub trait Connection {
	/// The ref advertisement the server sent when the connection opened — raw pkt-lines through the
	/// closing flush, exactly as [`parse_advertisement`](gitana_git_http::parse_advertisement) expects
	/// (SSH omits the `# service=` banner Smart HTTP prepends, which that parser already tolerates).
	fn advertisement(&self) -> &[u8];

	/// Send one request `body` (a pkt-line-encoded want/have negotiation or receive-pack command list)
	/// and return the server's full response bytes.
	fn exchange(&mut self, body: Vec<u8>) -> impl Future<Output = Result<Vec<u8>>>;

	/// End the session cleanly and surface any transport failure. For SSH this sends the terminating
	/// flush the client still owes when no request was made (an *empty* clone requests nothing — without
	/// it the server logs "the remote end hung up unexpectedly"), then awaits `ssh` and fails on a
	/// nonzero exit. For stateless HTTP there is nothing to finalise, so it is a no-op.
	fn finish(&mut self) -> impl Future<Output = Result<()>>;
}
