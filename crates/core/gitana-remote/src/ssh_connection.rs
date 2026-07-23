//! The native [`PackStream`] — an `ssh` subprocess whose stdio carries the git pack protocol.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::{PackConnection, PackStream, SshCommand, SshCommandKind, SshRemote, SshVariant};

/// A [`PackStream`] over an `ssh` subprocess — gitana as an SSH Git *client*. Like git, gitana never
/// links an SSH library; it drives the user's `ssh` binary (so `~/.ssh/config`, the agent, and
/// known-hosts all apply), keeping stderr on the terminal for host-key and passphrase prompts.
pub struct ChildStream {
	// Held to await ssh's exit status in `await_exit`, and reaped (kill-on-drop) if the connection is
	// dropped early; stdin stays open across rounds of the multi-round fetch negotiation.
	child: Child,
	stdin: ChildStdin,
	stdout: ChildStdout,
}

impl ChildStream {
	/// Spawn `ssh …host git-<service> '<path>'`, piping its stdin/stdout. `ssh` is the caller-resolved
	/// [`SshCommand`] (git's `GIT_SSH_COMMAND` / `core.sshCommand` / `GIT_SSH` precedence and the port-flag
	/// [`SshVariant`]); `cwd` is the directory the subprocess runs from (git runs ssh from the command's
	/// working directory), so a relative command or key path resolves against `gta`'s effective `-C`
	/// directory.
	async fn spawn(ssh: &SshCommand, remote: &SshRemote, service: &str, cwd: &Path) -> Result<Self> {
		let mut command = ssh_command(ssh, remote, service)?;
		command.current_dir(cwd);
		command.stdin(Stdio::piped()).stdout(Stdio::piped());
		// Reap the ssh process if the connection is dropped before the exchange completes (e.g. the
		// advertisement read fails) — otherwise an ssh blocked on auth would linger.
		command.kill_on_drop(true);
		let mut child = command.spawn().context("spawning ssh")?;
		let stdin = child.stdin.take().expect("ssh stdin was piped");
		let stdout = child.stdout.take().expect("ssh stdout was piped");
		Ok(Self {
			child,
			stdin,
			stdout,
		})
	}
}

impl PackStream for ChildStream {
	async fn write(&mut self, bytes: &[u8]) -> Result<()> {
		self
			.stdin
			.write_all(bytes)
			.await
			.context("writing to ssh")?;
		self.stdin.flush().await.context("flushing the ssh request")
	}

	async fn shutdown_write(&mut self) -> Result<()> {
		self.stdin.shutdown().await.context("closing the ssh stdin")
	}

	async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
		self
			.stdout
			.read_exact(buf)
			.await
			.context("reading from ssh")?;
		Ok(())
	}

	async fn read_to_end(&mut self) -> Result<Vec<u8>> {
		let mut out = Vec::new();
		self
			.stdout
			.read_to_end(&mut out)
			.await
			.context("reading the pack from ssh")?;
		Ok(out)
	}

	async fn await_exit(&mut self) -> Result<()> {
		// A wrapper (or ssh) may produce a complete, parseable pack and then fail; stock git reports a
		// transport error for that, so a nonzero exit must not read as success.
		let status = self.child.wait().await.context("waiting for ssh to exit")?;
		if !status.success() {
			bail!("ssh exited with {status}");
		}
		Ok(())
	}
}

impl PackConnection<ChildStream> {
	/// Open a native SSH connection to `service` (`git-upload-pack` / `git-receive-pack`) on `remote`,
	/// reading the ref advertisement the server sends immediately after the remote command starts. See
	/// [`ChildStream::spawn`] for `ssh` / `cwd`.
	pub async fn open(
		remote: &SshRemote,
		service: &str,
		ssh: &SshCommand,
		cwd: &Path,
	) -> Result<Self> {
		let stream = ChildStream::spawn(ssh, remote, service, cwd).await?;
		PackConnection::open_over(stream).await
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn single_quotes_paths_git_style() {
		assert_eq!(sq_quote("/srv/repo.git"), "'/srv/repo.git'");
		// An embedded single quote is closed, escaped, and reopened.
		assert_eq!(sq_quote("a'b"), "'a'\\''b'");
	}
}
