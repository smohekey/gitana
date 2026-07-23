//! In-guest SSH [`PackStream`] over the host-granted `ssh-transport` capability.
//!
//! The component cannot spawn `ssh` — it holds no subprocess authority — so the **host** spawns it
//! behind the `ssh-transport` WIT import and bridges the subprocess's stdio into a pair of `wasi:io`
//! streams, exactly as `wasi:http` backs the Smart HTTP porcelain. This type wraps those streams as a
//! [`PackStream`], so the shared pkt-line / `multi_ack_detailed` negotiation in `gitana-remote`
//! (`PackConnection` / `SshPackFetcher`) drives an SSH remote from inside the component unchanged.
//!
//! Every method is synchronous under the hood — it blocks on the `wasi:io` streams inline
//! ([`InputStream::blocking_read`], [`OutputStream::blocking_write_and_flush`]) and returns an
//! already-`Ready` future — so the component's noop-waker [`block_on`](crate::block_on) drives it
//! without ever seeing `Pending`, exactly as [`WasiHttpTransport`](crate::wasi_http_transport) does.
//! `blocking_write_and_flush` durably lands each chunk in the subprocess's stdin pipe before returning,
//! so closing the request side (dropping the output-stream in [`shutdown_write`](PackStream::shutdown_write))
//! never truncates a written request.

use anyhow::{Result, anyhow, bail};
use gitana_remote::{PackStream, SshRemote};
use wasip2::io::streams::{InputStream, OutputStream, StreamError};

use crate::bindings::gitana::repo::ssh_transport::{self, SshSession};

/// The most bytes a single `blocking-write-and-flush` may accept (WASI 0.2 caps it at 4096).
const WRITE_CHUNK: usize = 4096;
/// Response read granularity for [`read_to_end`](PackStream::read_to_end).
const READ_CHUNK: u64 = 64 * 1024;

/// A [`PackStream`] over the host-granted SSH session: the subprocess's stdout is read for the
/// advertisement / ACKs / packfile, its stdin written with the client's request. Holds the `ssh-session`
/// resource so [`await_exit`](PackStream::await_exit) can await the subprocess's exit status.
pub(crate) struct WasiSshStream {
	session: SshSession,
	/// The request side; `None` once [`shutdown_write`](PackStream::shutdown_write) has closed it (dropping
	/// the output-stream signals end-of-request to the server).
	stdin: Option<OutputStream>,
	stdout: InputStream,
}

impl WasiSshStream {
	/// Open an SSH session running `git-<service>` (`git-upload-pack` / `git-receive-pack`) on `remote`
	/// over the host `ssh-transport` capability, taking the subprocess's stdin/stdout streams. The ref
	/// advertisement is not read here — [`PackConnection::open_over`](gitana_remote::PackConnection) reads
	/// it from `stdout` as its first act.
	pub(crate) fn open(service: &str, remote: &SshRemote) -> Result<Self> {
		let session = ssh_transport::open(
			service,
			&remote.host,
			remote.port,
			remote.user.as_deref(),
			&remote.path,
		)
		.map_err(|e| anyhow!("opening ssh session: {e}"))?;
		let stdout = session.stdout();
		let stdin = session.stdin();
		Ok(Self {
			session,
			stdin: Some(stdin),
			stdout,
		})
	}
}

impl PackStream for WasiSshStream {
	async fn write(&mut self, bytes: &[u8]) -> Result<()> {
		let stdin = self
			.stdin
			.as_ref()
			.ok_or_else(|| anyhow!("write after the ssh request side was closed"))?;
		// WASI's blocking-write-and-flush accepts at most 4 KiB per call and blocks until each chunk has
		// been flushed into the subprocess's stdin pipe.
		for chunk in bytes.chunks(WRITE_CHUNK) {
			stdin
				.blocking_write_and_flush(chunk)
				.map_err(|e| anyhow!("writing to ssh: {e:?}"))?;
		}
		Ok(())
	}

	async fn shutdown_write(&mut self) -> Result<()> {
		// Drop the output-stream: the host closes the subprocess's stdin, signalling end-of-request so the
		// server proceeds to its response. All prior writes were flushed, so nothing is truncated.
		self.stdin = None;
		Ok(())
	}

	async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
		let mut filled = 0;
		while filled < buf.len() {
			let want = (buf.len() - filled) as u64;
			match self.stdout.blocking_read(want) {
				// `blocking_read` returns at most `want` bytes, blocking until at least one is available or
				// the stream closes — so an early close before `buf` is full is an unexpected EOF.
				Ok(chunk) => {
					let end = filled + chunk.len();
					buf[filled..end].copy_from_slice(&chunk);
					filled = end;
				}
				Err(StreamError::Closed) => {
					bail!("ssh stream closed after {filled} of {} bytes", buf.len());
				}
				Err(StreamError::LastOperationFailed(e)) => {
					bail!("reading from ssh: {}", e.to_debug_string());
				}
			}
		}
		Ok(())
	}

	async fn read_to_end(&mut self) -> Result<Vec<u8>> {
		let mut out = Vec::new();
		loop {
			match self.stdout.blocking_read(READ_CHUNK) {
				Ok(chunk) => out.extend_from_slice(&chunk),
				Err(StreamError::Closed) => break,
				Err(StreamError::LastOperationFailed(e)) => {
					bail!("reading the pack from ssh: {}", e.to_debug_string());
				}
			}
		}
		Ok(out)
	}

	async fn await_exit(&mut self) -> Result<()> {
		// A wrapper (or ssh) may produce a complete, parseable pack and then fail; stock git reports a
		// transport error for that, so a nonzero exit must not read as success. The host awaits the child.
		self
			.session
			.finish()
			.map_err(|e| anyhow!("ssh transport failed: {e}"))
	}
}
