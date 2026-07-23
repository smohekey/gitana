//! The pluggable backend behind the host's imported `ssh-transport` capability.

use tokio::process::Command;

/// Answers the guest's `ssh-transport` import — the host side of git's SSH transport. [`State`] holds
/// one of these and forwards the WIT `open` to it, so an embedder owns all ssh *policy* (which `ssh`
/// binary, keys, `~/.ssh/config`, variant, host resolution) without touching the wasmtime wiring — the
/// same capability model the `credentials` import follows. A `State` built with no provider fails every
/// `open`, so a component granted no SSH capability simply cannot reach an SSH remote (the guest's own
/// error, not a panic).
///
/// The provider *builds* the `ssh` invocation; the **host spawns it**. This split keeps every
/// safety- and transport-critical concern under the host's control, so a provider cannot get one wrong:
///
/// - the host validates the request (service allow-list, option-injection guard) *before* calling in;
/// - the host assembles and shell-escapes the remote command `git-<service> '<path>'` (git's `sq_quote`),
///   so a repository path containing shell syntax cannot inject into the remote shell — the provider
///   receives an already-safe `remote_command` to hand to `ssh`;
/// - the host pipes stdin/stdout (to bridge into the guest's `wasi:io` streams), sets kill-on-drop (so a
///   session dropped before `finish` reaps its `ssh`), and clears `GIT_PROTOCOL` so the server stays on
///   protocol v0 (gitana is a v0 client).
///
/// The provider is left with pure ssh *policy*: which `ssh` binary and variant, keys, `~/.ssh/config`,
/// host resolution, and the port flag. Spawning `ssh` from the guest itself is impossible (a component
/// holds no subprocess authority), which is exactly why this is a host capability.
///
/// [`State`]: crate::State
pub trait HostSshProvider: Send + Sync {
	/// Build the `ssh` invocation that connects to `host` (refined by `port`/`user`) and runs
	/// `remote_command` on it. `remote_command` is the host-assembled, already shell-escaped
	/// `git-<service> '<path>'` (`git-upload-pack` for fetch/clone, `git-receive-pack` for push) — the
	/// provider appends it verbatim as `ssh`'s command argument and must not re-quote or otherwise alter
	/// it. Set the program, connection arguments (port flag per variant, `[user@]host`), working
	/// directory, and any environment the ssh policy needs, but do **not** spawn the command or configure
	/// its stdio, `kill_on_drop`, or `GIT_PROTOCOL`: the host applies those to the returned [`Command`] and
	/// spawns it. Return a human-readable error if the command cannot be built (e.g. the request is
	/// unsupported by this provider).
	fn open(
		&self,
		host: &str,
		port: Option<u16>,
		user: Option<&str>,
		remote_command: &str,
	) -> Result<Command, String>;
}
