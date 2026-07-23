//! How gitana invokes `ssh` for an SSH remote — the resolved command plus its [`SshVariant`].

use crate::SshVariant;

/// The resolved `ssh` invocation for an SSH remote, as the caller derived it from git's
/// `GIT_SSH_COMMAND` / `core.sshCommand` / `GIT_SSH` precedence: either a **shell command**
/// (`GIT_SSH_COMMAND` / `core.sshCommand`, run through `sh -c`) or a **program** (`GIT_SSH`, or the
/// default `ssh`, run directly), together with the [`SshVariant`] that decides the port flag.
/// [`SshConnection::open`](crate::SshConnection::open) consumes this instead of reading the environment
/// itself, so the config-aware resolution lives in the frontend that holds the git config.
pub struct SshCommand {
	pub(crate) kind: SshCommandKind,
	pub(crate) variant: SshVariant,
}

/// Whether the ssh command runs through a shell or as a direct program.
pub(crate) enum SshCommandKind {
	/// A shell command (`GIT_SSH_COMMAND` / `core.sshCommand`), run as `sh -c '<cmd> "$@"' ssh <args>`.
	Shell(String),
	/// A program path (`GIT_SSH`, or the default `ssh`), executed directly with the ssh arguments.
	Program(String),
}

impl SshCommand {
	/// A shell-style ssh command (`GIT_SSH_COMMAND` / `core.sshCommand`).
	pub fn shell(command: String, variant: SshVariant) -> Self {
		Self {
			kind: SshCommandKind::Shell(command),
			variant,
		}
	}

	/// A program-style ssh command (`GIT_SSH`, or the default `ssh` binary).
	pub fn program(program: String, variant: SshVariant) -> Self {
		Self {
			kind: SshCommandKind::Program(program),
			variant,
		}
	}
}
