//! A [`Connection`] over an `ssh` subprocess — gitana as an SSH Git *client*.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::{Connection, SshCommand, SshCommandKind, SshRemote, SshVariant};

/// A single SSH session: spawn `ssh …host git-<service> '<path>'`, read the ref advertisement the
/// remote sends on connect, then exchange the pack negotiation over the child's stdin/stdout. Like git,
/// gitana never links an SSH library — it drives the user's `ssh` binary (so `~/.ssh/config`, the agent,
/// and known-hosts all apply), keeping stderr on the terminal for host-key and passphrase prompts.
pub struct SshConnection {
	// Held to await ssh's exit status in `finish`, and reaped (kill-on-drop) if the connection is
	// dropped early; stdin stays open across rounds of the multi-round fetch negotiation.
	child: Child,
	stdin: ChildStdin,
	stdout: ChildStdout,
	advertisement: Vec<u8>,
	/// Whether a request was written (so `finish` knows whether it still owes the terminating flush an
	/// empty clone never sent).
	request_sent: bool,
}

impl SshConnection {
	/// Open a connection to `service` (`git-upload-pack` / `git-receive-pack`) on `remote`, reading the
	/// ref advertisement the server sends immediately after the remote command starts. `ssh` is the
	/// caller-resolved [`SshCommand`] (git's `GIT_SSH_COMMAND` / `core.sshCommand` / `GIT_SSH` precedence
	/// and the port-flag [`SshVariant`]). `cwd` is the directory the subprocess runs from (git runs ssh
	/// from the command's working directory), so a relative command or key path resolves against `gta`'s
	/// effective `-C` directory.
	pub async fn open(
		remote: &SshRemote,
		service: &str,
		ssh: &SshCommand,
		cwd: &Path,
	) -> Result<Self> {
		let mut command = ssh_command(ssh, remote, service)?;
		command.current_dir(cwd);
		command.stdin(Stdio::piped()).stdout(Stdio::piped());
		// Reap the ssh process if the connection is dropped before the exchange completes (e.g. the
		// advertisement read fails) — otherwise an ssh blocked on auth would linger. On the happy path ssh
		// has already exited (we read its stdout to EOF), so this is a no-op there.
		command.kill_on_drop(true);
		let mut child = command.spawn().context("spawning ssh")?;
		let stdin = child.stdin.take().expect("ssh stdin was piped");
		let mut stdout = child.stdout.take().expect("ssh stdout was piped");
		let advertisement = read_advertisement(&mut stdout)
			.await
			.context("reading the ssh ref advertisement")?;
		Ok(Self {
			child,
			stdin,
			stdout,
			advertisement,
			request_sent: false,
		})
	}

	/// Write one negotiation message (pkt-lines) without closing stdin — for a multi-round fetch, where
	/// stdin stays open between rounds and is closed only in [`read_pack`](Self::read_pack) after `done`.
	pub(crate) async fn write(&mut self, bytes: &[u8]) -> Result<()> {
		self
			.stdin
			.write_all(bytes)
			.await
			.context("writing to ssh")?;
		self
			.stdin
			.flush()
			.await
			.context("flushing the ssh request")?;
		self.request_sent = true;
		Ok(())
	}

	/// Read one acknowledgment batch (the server's response to a have-group) and report whether the
	/// client should now send `done`. The batch boundary depends on the negotiation mode the server chose
	/// from what we advertised:
	/// - `multi_ack_detailed`: `ACK <oid> common`* then optionally `ACK <oid> ready`, terminated by `NAK`.
	///   `ready` means the server has a sufficient cut point; the boundary is the trailing `NAK`.
	/// - plain `multi_ack`: `ACK <oid> continue`* terminated by `NAK` (no `ready` — keep offering haves).
	/// - single-ack (base protocol v0): a bare `ACK <oid>` is terminal — the server then stays silent
	///   until `done`, so it must end the batch, or the read deadlocks.
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
							// multi_ack_detailed: a `NAK` still terminates this batch.
							AckClass::Ready => ready = true,
							// A bare `ACK <oid>` — single-ack base protocol, which sends no terminating `NAK`.
							AckClass::BareTerminal => return Ok(true),
							// `common` / `continue`: consumed; keep reading to the terminating `NAK`.
							AckClass::Partial => {}
						}
					}
				}
			}
		}
	}

	/// Close stdin (git closes the child's input after `done`) and read the final response — the last
	/// `ACK` and the side-band packfile — to EOF, for [`parse_upload_pack_response`].
	pub(crate) async fn read_pack(&mut self) -> Result<Vec<u8>> {
		self
			.stdin
			.shutdown()
			.await
			.context("closing the ssh stdin")?;
		let mut response = Vec::new();
		self
			.stdout
			.read_to_end(&mut response)
			.await
			.context("reading the pack from ssh")?;
		Ok(response)
	}

	// (see `ack_class` below for the per-line classification)

	/// Read one pkt-line's payload from stdout, or `None` on a flush-pkt (`0000`).
	async fn read_pkt_line(&mut self) -> Result<Option<Vec<u8>>> {
		let mut len_bytes = [0u8; 4];
		self
			.stdout
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
			.stdout
			.read_exact(&mut body)
			.await
			.context("reading a pkt-line body")?;
		Ok(Some(body))
	}
}

impl Connection for SshConnection {
	fn advertisement(&self) -> &[u8] {
		&self.advertisement
	}

	async fn exchange(&mut self, body: Vec<u8>) -> Result<Vec<u8>> {
		self
			.stdin
			.write_all(&body)
			.await
			.context("writing the upload-pack request to ssh")?;
		self
			.stdin
			.flush()
			.await
			.context("flushing the ssh request")?;
		self.request_sent = true;
		// Close stdin after the request (git closes the child's input after `done`), so a wrapper that
		// waits for stdin EOF before finishing its stdout does not deadlock our `read_to_end`. `exchange`
		// is the single-round shape (clone / push / receive-pack); the multi-round fetch keeps stdin open
		// between rounds via `write`/`read_ack_batch`, closing it only in `read_pack`.
		self
			.stdin
			.shutdown()
			.await
			.context("closing the ssh stdin")?;
		// A single-round exchange: after the request, the server streams its whole response and closes its
		// stdout, so reading to EOF collects it. The child's exit status is checked in `finish`.
		let mut response = Vec::new();
		self
			.stdout
			.read_to_end(&mut response)
			.await
			.context("reading the pack from ssh")?;
		Ok(response)
	}

	async fn finish(&mut self) -> Result<()> {
		// An empty clone requests nothing, so no `exchange` ran and upload-pack is still waiting on the
		// client — send the terminating flush-pkt (`0000`) git owes it, so it exits cleanly instead of
		// logging "the remote end hung up unexpectedly" when we drop the connection.
		if !self.request_sent {
			self
				.stdin
				.write_all(b"0000")
				.await
				.context("sending the terminating flush to ssh")?;
			self
				.stdin
				.flush()
				.await
				.context("flushing the ssh flush-pkt")?;
			// Close stdin so upload-pack sees EOF and exits (a wrapper waiting on stdin EOF would else hang).
			self
				.stdin
				.shutdown()
				.await
				.context("closing the ssh stdin")?;
		}
		// Await ssh's exit and propagate a nonzero status: a wrapper (or ssh) may produce a complete,
		// parseable pack and then fail, and stock git reports a transport error for that — a nonzero exit
		// must not read as a successful clone.
		let status = self.child.wait().await.context("waiting for ssh to exit")?;
		if !status.success() {
			bail!("ssh exited with {status}");
		}
		Ok(())
	}
}

/// Build the `ssh` invocation for `service` on `remote` from the resolved [`SshCommand`]. A shell
/// command runs as `sh -c '<cmd> "$@"' ssh <args>`; a program runs directly. The port flag follows the
/// [`SshVariant`]: `-p` for OpenSSH, `-P` for the PuTTY family, and none for `simple` (which errors if a
/// port is set). The remote command is `git-<service> '<path>'`, the path single-quoted for the remote
/// shell, matching git.
fn ssh_command(ssh: &SshCommand, remote: &SshRemote, service: &str) -> Result<Command> {
	if remote.port.is_some() && ssh.variant == SshVariant::Simple {
		bail!("ssh variant 'simple' does not support setting a port");
	}
	let remote_command = format!("{service} {}", sq_quote(&remote.path));
	let target = match &remote.user {
		Some(user) => format!("{user}@{}", remote.host),
		None => remote.host.clone(),
	};

	let mut command = match &ssh.kind {
		// git treats an explicitly *empty* command as authoritative and fails rather than falling back to
		// `ssh` — so a `GIT_SSH_COMMAND=` / `core.sshCommand=` / `GIT_SSH=` set to disable ssh never makes
		// an unexpected connection.
		SshCommandKind::Shell(command) if command.is_empty() => {
			bail!("the ssh command is set but empty");
		}
		SshCommandKind::Program(program) if program.is_empty() => {
			bail!("the ssh command is set but empty");
		}
		SshCommandKind::Shell(command) => {
			// git runs the custom command through a shell: `sh -c '<cmd> "$@"' ssh <ssh args…>`.
			let mut process = Command::new("sh");
			process.arg("-c");
			process.arg(format!("{command} \"$@\""));
			process.arg("ssh");
			push_ssh_args(
				&mut process,
				ssh.variant,
				remote.port,
				&target,
				&remote_command,
			);
			process
		}
		SshCommandKind::Program(program) => {
			let mut process = Command::new(program);
			push_ssh_args(
				&mut process,
				ssh.variant,
				remote.port,
				&target,
				&remote_command,
			);
			process
		}
	};
	// gitana speaks Git protocol v0 only. We never send `-o SendEnv=GIT_PROTOCOL`, so a plain `ssh`
	// won't forward the variable, but a shell command that runs upload-pack directly would inherit it —
	// clear it so an ambient `GIT_PROTOCOL=version=2` can't make the server answer in v2.
	command.env_remove("GIT_PROTOCOL");
	Ok(command)
}

fn push_ssh_args(
	command: &mut Command,
	variant: SshVariant,
	port: Option<u16>,
	target: &str,
	remote_command: &str,
) {
	// TortoisePlink runs `-batch` so an unattended fetch/push never blocks on an interactive dialog.
	if variant == SshVariant::TortoisePlink {
		command.arg("-batch");
	}
	if let Some(port) = port {
		// OpenSSH uses `-p`, the PuTTY family `-P`; `simple` was rejected above.
		let flag = match variant {
			SshVariant::Putty | SshVariant::TortoisePlink => "-P",
			_ => "-p",
		};
		command.arg(flag).arg(port.to_string());
	}
	command.arg(target);
	command.arg(remote_command);
}

/// POSIX single-quote `s` for the remote shell — wrap in `'…'`, rendering an embedded `'` as `'\''`
/// (git's `sq_quote`), so a path with shell metacharacters reaches `git-upload-pack` intact.
fn sq_quote(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 2);
	out.push('\'');
	for ch in s.chars() {
		if ch == '\'' {
			out.push_str("'\\''");
		} else {
			out.push(ch);
		}
	}
	out.push('\'');
	out
}

/// Read the v0 ref advertisement: successive pkt-lines up to and including the terminating flush-pkt
/// (`0000`), returning the raw bytes (banner-free, as SSH sends them) for `parse_advertisement`.
async fn read_advertisement(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
	let mut out = Vec::new();
	loop {
		let mut len_bytes = [0u8; 4];
		reader
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
			// The flush-pkt terminates the ref advertisement.
			break;
		}
		if len < 4 {
			bail!("invalid pkt-line length {len}");
		}
		let mut body = vec![0u8; len - 4];
		reader
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
		// multi_ack_detailed `ready`, multi_ack `common`/`continue`, and a bare single-ack `ACK`.
		assert!(matches!(ack_class(b"abc123 ready"), AckClass::Ready));
		assert!(matches!(ack_class(b"abc123 common"), AckClass::Partial));
		assert!(matches!(ack_class(b"abc123 continue"), AckClass::Partial));
		assert!(matches!(ack_class(b"abc123"), AckClass::BareTerminal));
	}

	#[test]
	fn single_quotes_paths_git_style() {
		assert_eq!(sq_quote("/srv/repo.git"), "'/srv/repo.git'");
		// An embedded single quote is closed, escaped, and reopened.
		assert_eq!(sq_quote("a'b"), "'a'\\''b'");
	}

	/// Frame `payload` as one pkt-line (4-hex length prefix including the prefix itself).
	fn pkt(payload: &str) -> Vec<u8> {
		let len = payload.len() + 4;
		format!("{len:04x}{payload}").into_bytes()
	}

	#[tokio::test]
	async fn reads_advertisement_through_the_flush() {
		// A ref line, then a flush-pkt; bytes after the flush must not be consumed.
		let mut advert = pkt("9f5829a8c0e5b1e9f0e7d3c1a2b3c4d5e6f7a8b9 refs/heads/main\0caps\n");
		advert.extend_from_slice(b"0000trailing");
		let mut cursor = std::io::Cursor::new(advert);
		let read = read_advertisement(&mut cursor).await.unwrap();
		assert!(read.ends_with(b"0000"));
		assert!(read.windows(4).any(|w| w == b"caps"));
		// The `trailing` bytes remain unread in the stream.
		let mut rest = Vec::new();
		tokio::io::AsyncReadExt::read_to_end(&mut cursor, &mut rest)
			.await
			.unwrap();
		assert_eq!(rest, b"trailing");
	}
}
