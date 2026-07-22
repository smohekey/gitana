//! A [`Connection`] over an `ssh` subprocess — gitana as an SSH Git *client*.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::{Connection, SshRemote};

/// A single SSH session: spawn `ssh …host git-<service> '<path>'`, read the ref advertisement the
/// remote sends on connect, then exchange the pack negotiation over the child's stdin/stdout. Like git,
/// gitana never links an SSH library — it drives the user's `ssh` binary (so `~/.ssh/config`, the agent,
/// and known-hosts all apply), keeping stderr on the terminal for host-key and passphrase prompts.
pub struct SshConnection {
	// Held to await ssh's exit status in `finish`, and reaped (kill-on-drop) if the connection is
	// dropped early; stdin is kept for the multi-round fetch negotiation a later slice adds.
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
	/// ref advertisement the server sends immediately after the remote command starts. `cwd` is the
	/// directory the `ssh` subprocess runs from (git runs ssh from the command's working directory), so a
	/// relative `GIT_SSH_COMMAND` or key path resolves against `gta`'s effective `-C` directory.
	pub async fn open(remote: &SshRemote, service: &str, cwd: &Path) -> Result<Self> {
		let mut command = ssh_command(remote, service)?;
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
		// waits for stdin EOF before finishing its stdout does not deadlock our `read_to_end`. This is the
		// final round for a clone; multi-round fetch (a later slice) closes stdin only after its last round.
		self
			.stdin
			.shutdown()
			.await
			.context("closing the ssh stdin")?;
		// A full clone is a single negotiation round: after `done`, upload-pack streams the pack and
		// closes its stdout, so reading to EOF collects the whole response. Multi-round fetch negotiation
		// — which must stop at a boundary rather than EOF — is a later slice. The child's exit status is
		// checked in `finish`.
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

/// Build the `ssh` invocation for `service` on `remote`. Program resolution follows a subset of git's
/// precedence: `GIT_SSH_COMMAND` (a shell snippet) then the `ssh` binary on `PATH`. (`core.sshCommand`,
/// `GIT_SSH`, and the PuTTY/plink `-P` port variant are a later slice.) The remote command is
/// `git-<service> '<path>'` with the path single-quoted for the remote shell, matching git.
fn ssh_command(remote: &SshRemote, service: &str) -> Result<Command> {
	let remote_command = format!("{service} {}", sq_quote(&remote.path));
	let target = match &remote.user {
		Some(user) => format!("{user}@{}", remote.host),
		None => remote.host.clone(),
	};

	let mut command = match std::env::var_os("GIT_SSH_COMMAND") {
		// git treats an explicitly *empty* `GIT_SSH_COMMAND` as authoritative and fails rather than
		// falling back to `ssh` — so an env set to disable ssh never makes an unexpected connection.
		Some(ssh_command) if ssh_command.is_empty() => {
			bail!("GIT_SSH_COMMAND is set but empty");
		}
		Some(ssh_command) => {
			// git runs the custom command through a shell: `sh -c '<cmd> "$@"' <cmd> <ssh args…>`.
			let mut command = Command::new("sh");
			command.arg("-c");
			command.arg(format!("{} \"$@\"", ssh_command.to_string_lossy()));
			command.arg("ssh");
			push_ssh_args(&mut command, remote.port, &target, &remote_command);
			command
		}
		None => {
			let mut command = Command::new("ssh");
			push_ssh_args(&mut command, remote.port, &target, &remote_command);
			command
		}
	};
	// gitana speaks Git protocol v0 only. We never send `-o SendEnv=GIT_PROTOCOL`, so a plain `ssh`
	// won't forward the variable, but a `GIT_SSH_COMMAND` wrapper that runs upload-pack directly would
	// inherit it — clear it so an ambient `GIT_PROTOCOL=version=2` can't make the server answer in v2.
	command.env_remove("GIT_PROTOCOL");
	Ok(command)
}

fn push_ssh_args(command: &mut Command, port: Option<u16>, target: &str, remote_command: &str) {
	if let Some(port) = port {
		command.arg("-p").arg(port.to_string());
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

#[cfg(test)]
mod tests {
	use super::*;

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
