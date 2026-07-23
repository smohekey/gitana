//! Resolving how gitana invokes `ssh`, matching git's precedence and variant detection.

use anyhow::{Result, bail};
use gitana_config::GitConfig;
use gitana_remote::{SshCommand, SshVariant};

/// Resolve the `ssh` invocation for an SSH remote from git's precedence:
/// `GIT_SSH_COMMAND` (env) → `core.sshCommand` (config) → `GIT_SSH` (env) → the `ssh` binary. The first
/// two are **shell** commands (run through `sh -c`); the latter two are **programs** (run directly).
///
/// A present-but-empty value at any of those levels is **authoritative** — the connection layer then
/// fails rather than falling through to a lower level or the default `ssh`, so an override set to
/// disable ssh never makes an unexpected connection (matching git).
///
/// The port-flag [`SshVariant`] follows `GIT_SSH_VARIANT` (env) → `ssh.variant` (config) → basename
/// auto-detection of the resolved command. Each *present* level is authoritative: an explicit `auto`
/// (or unknown) value means "auto-detect by basename", not "consult the next level".
pub fn resolve_ssh_command(config: &GitConfig) -> Result<SshCommand> {
	let (command, is_shell) = if let Some(env) = std::env::var_os("GIT_SSH_COMMAND") {
		(env.to_string_lossy().into_owned(), true)
	} else if let Some(core) = config_string(config, "core", "sshcommand")? {
		(core.to_owned(), true)
	} else if let Some(env) = std::env::var_os("GIT_SSH") {
		(env.to_string_lossy().into_owned(), false)
	} else {
		("ssh".to_owned(), false)
	};

	// The program whose basename decides an auto-detected variant: a program's whole path, or a shell
	// command's first word (unescaped/unquoted, since a shell command runs through `sh`).
	let program = if is_shell {
		first_shell_word(&command)
	} else {
		command.clone()
	};
	let variant = if let Some(env) = std::env::var_os("GIT_SSH_VARIANT") {
		let env = env.to_string_lossy();
		SshVariant::parse(&env).unwrap_or_else(|| SshVariant::detect(&program))
	} else if let Some(cfg) = config_string(config, "ssh", "variant")? {
		SshVariant::parse(cfg).unwrap_or_else(|| SshVariant::detect(&program))
	} else {
		SshVariant::detect(&program)
	};

	Ok(if is_shell {
		SshCommand::shell(command, variant)
	} else {
		SshCommand::program(command, variant)
	})
}

/// Read a single-valued `<section>.<key>` from `config`, distinguishing **absent** (`Ok(None)`, so the
/// caller falls through) from a **valueless** key — a bare `[section] key` with no `=` — which git
/// rejects with "missing value for '<section>.<key>'". The last entry wins (single-valued), matching
/// git. An empty *value* (`key =`) is a real value (`Ok(Some(""))`), authoritative below.
fn config_string<'a>(config: &'a GitConfig, section: &str, key: &str) -> Result<Option<&'a str>> {
	let mut effective: Option<Option<&str>> = None;
	for (subsection, value) in config.variables_named(section, key) {
		if subsection.is_none() {
			effective = Some(value);
		}
	}
	match effective {
		None => Ok(None),
		Some(None) => bail!("missing value for '{section}.{key}'"),
		Some(Some(value)) => Ok(Some(value)),
	}
}

/// The first word of a shell command — its executable — as `sh` would tokenize it, so its basename can
/// be classified. Honours the POSIX quoting a command runs under: a `\<char>` escape (so
/// `/opt/PuTTY\ Tools/plink` yields `/opt/PuTTY Tools/plink`), and `'…'` / `"…"` quoting (so
/// `"C:\Program Files\plink.exe" -x` keeps its backslashes and spaces). Whitespace outside a quote ends
/// the word. (This is the shell path only; a `GIT_SSH` program is used verbatim, never tokenized.)
fn first_shell_word(command: &str) -> String {
	let mut chars = command.trim_start().chars();
	let mut word = String::new();
	while let Some(c) = chars.next() {
		match c {
			// An unquoted backslash escapes the next character.
			'\\' => {
				if let Some(next) = chars.next() {
					word.push(next);
				}
			}
			// A quoted run is literal up to the matching quote (backslashes inside are kept, as a Windows
			// path needs).
			'\'' | '"' => {
				for quoted in chars.by_ref() {
					if quoted == c {
						break;
					}
					word.push(quoted);
				}
			}
			c if c.is_whitespace() => break,
			c => word.push(c),
		}
	}
	word
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn first_word_respects_quotes_and_escapes() {
		assert_eq!(first_shell_word("plink -x"), "plink");
		// A quoted path keeps its spaces and backslashes.
		assert_eq!(
			first_shell_word("\"C:\\Program Files\\PuTTY\\plink.exe\" -x"),
			"C:\\Program Files\\PuTTY\\plink.exe"
		);
		// A backslash-escaped space (the shell unescapes it) is part of the word.
		assert_eq!(
			first_shell_word("/opt/PuTTY\\ Tools/plink -4"),
			"/opt/PuTTY Tools/plink"
		);
		assert_eq!(first_shell_word("  ssh  "), "ssh");
	}

	#[test]
	fn config_string_distinguishes_absent_valueless_and_empty() {
		let cfg = |text: &str| GitConfig::parse(text).unwrap();
		// Absent → None (fall through).
		assert_eq!(config_string(&cfg(""), "core", "sshcommand").unwrap(), None);
		// Valueless (`sshCommand` with no `=`) → error, as git aborts.
		assert!(config_string(&cfg("[core]\n\tsshCommand\n"), "core", "sshcommand").is_err());
		// An empty value (`sshCommand =`) is a real, authoritative value.
		assert_eq!(
			config_string(&cfg("[core]\n\tsshCommand =\n"), "core", "sshcommand").unwrap(),
			Some("")
		);
		// A set value wins, and the last one is effective.
		assert_eq!(
			config_string(
				&cfg("[core]\n\tsshCommand = a\n\tsshCommand = plink\n"),
				"core",
				"sshcommand"
			)
			.unwrap(),
			Some("plink")
		);
	}
}
