//! Interactive credential prompting, git's way.
//!
//! Git resolves a prompter in a fixed order — `GIT_ASKPASS`, then `core.askPass`, then `SSH_ASKPASS`,
//! then the controlling terminal — and reads the answer from it. An askpass *program* is invoked as
//! `program "<prompt>"` and answers on stdout; the terminal path reads `/dev/tty` directly (so it
//! works even when stdin/stdout are redirected), turning off echo for secrets. When there is no
//! askpass and no usable terminal — or `GIT_TERMINAL_PROMPT` is a git-false value, or terminal prompts
//! are disabled for this task — prompting declines with `Ok(None)` so a headless caller fails on the
//! server's `401` rather than hanging.
//!
//! The `/dev/tty` fallback is unix-only; on other platforms only the askpass programs are available.

use std::future::Future;
use std::path::Path;

use anyhow::{Context, Result};
use tokio::process::Command;

tokio::task_local! {
	/// Whether the controlling terminal may be used to prompt in this task. Unset (the `gta` CLI) means
	/// yes; the MCP frontend scopes it to `false` via [`with_terminal_prompts_disabled`] so an
	/// in-process command never blocks on `/dev/tty` waiting for input the MCP caller cannot provide.
	static TERMINAL_PROMPTS: bool;
}

/// Run `future` with the `/dev/tty` credential prompt disabled — for the MCP frontend, which has no
/// interactive user, so a command must never block waiting for terminal input. Askpass programs
/// (`GIT_ASKPASS` / `core.askPass` / `SSH_ASKPASS`) still work; only the terminal fallback is
/// suppressed, exactly as `GIT_TERMINAL_PROMPT=0` would.
pub async fn with_terminal_prompts_disabled<F: Future>(future: F) -> F::Output {
	TERMINAL_PROMPTS.scope(false, future).await
}

/// Whether the terminal fallback is permitted in this task (default yes, when unscoped).
fn terminal_prompts_allowed() -> bool {
	TERMINAL_PROMPTS
		.try_with(|allowed| *allowed)
		.unwrap_or(true)
}

/// Whether the typed input should be echoed (a username) or hidden (a password/token).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Echo {
	Show,
	Hide,
}

/// Prompt for one line with text `prompt`. `askpass` is the resolved askpass program (see
/// [`super::credential`]); when it is `None` — or the askpass program fails to run or exits
/// unsuccessfully — the controlling terminal is used, matching git's `git_prompt` (which sets
/// `r = do_askpass(...)` with no early return, then falls through to `git_terminal_prompt` when that
/// yields nothing). The terminal path itself declines under `GIT_TERMINAL_PROMPT`/no-tty, so an
/// unattended command does not block. Returns `Ok(None)` only when no prompter is available at all. An
/// empty answer from a *successful* prompt is `Some("")` (a present empty field), not `None`.
pub(crate) async fn ask(
	askpass: Option<&str>,
	prompt: &str,
	echo: Echo,
	cwd: &Path,
) -> Result<Option<String>> {
	if let Some(program) = askpass
		&& let Some(answer) = run_askpass(program, prompt, cwd).await
	{
		return Ok(Some(answer));
	}
	// No askpass configured, or it failed — git falls through to the controlling terminal.
	terminal(prompt, echo).await
}

/// Invoke an askpass `program` with `prompt` as its sole argument and take the first line of stdout as
/// the answer, matching git. The child runs from `cwd` — git executes the helper from the directory it
/// chdir'd to (the worktree root, or the launch directory for `clone`), so a relative helper resolves
/// as git resolves it. `None` means the program was unavailable (could not spawn, or exited
/// unsuccessfully) — git warns and the caller falls back to the terminal. `Some` (even empty) is a
/// present answer — an empty password is a *present* empty password (`user:`).
async fn run_askpass(program: &str, prompt: &str, cwd: &Path) -> Option<String> {
	let output = match Command::new(program)
		.arg(prompt)
		.current_dir(cwd)
		.output()
		.await
	{
		Ok(output) if output.status.success() => output,
		// A missing or failing askpass is not fatal — warn (as git does) and let the caller fall back.
		_ => {
			eprintln!("warning: unable to run askpass program '{program}'");
			return None;
		}
	};
	let answer = String::from_utf8(output.stdout).ok()?;
	Some(answer.lines().next().unwrap_or("").to_owned())
}

/// Prompt on the controlling terminal, hiding input for [`Echo::Hide`]. Reads from the TTY via
/// `rpassword`/`rprompt` (raw mode, so `Ctrl-C` is read as input and the terminal is restored — no
/// signal handler). Blocking, so it runs on a blocking thread. `Ok(None)` when terminal prompts are
/// disabled for this task (the MCP frontend), when `GIT_TERMINAL_PROMPT` is a git-false value, or when
/// there is no usable terminal (a headless process) — the clean "cannot prompt" signal.
async fn terminal(prompt: &str, echo: Echo) -> Result<Option<String>> {
	// The MCP frontend disables terminal prompts for its tasks; never block on the TTY there.
	if !terminal_prompts_allowed() {
		return Ok(None);
	}
	// git honours GIT_TERMINAL_PROMPT as a boolean: any false value (`0`, `false`, `no`, `off`, empty)
	// disables terminal prompting; an unparsable value is an error, as git aborts on a bad boolean.
	if let Ok(value) = std::env::var("GIT_TERMINAL_PROMPT") {
		let enabled = crate::git_config::parse_git_bool(&value)
			.ok_or_else(|| anyhow::anyhow!("bad boolean value '{value}' for 'GIT_TERMINAL_PROMPT'"))?;
		if !enabled {
			return Ok(None);
		}
	}
	// `rpassword`/`rprompt` open the TTY themselves (working even when stdin/stdout are redirected, as git
	// does) and manage raw mode. Run off-runtime so the blocking read never stalls the executor. A failure
	// to reach a terminal (headless) becomes `Ok(None)` — "cannot prompt" — so the caller declines rather
	// than aborts; `Ctrl-C` restores the terminal and re-raises `SIGINT`, ending the process cleanly.
	let prompt = prompt.to_owned();
	tokio::task::spawn_blocking(move || match echo {
		Echo::Hide => rpassword::prompt_password(&prompt).ok(),
		Echo::Show => rprompt::prompt_reply(&prompt).ok(),
	})
	.await
	.context("terminal prompt task panicked")
}
