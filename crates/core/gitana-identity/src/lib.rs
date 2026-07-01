//! Git author/committer identity resolution and formatting.
//!
//! A git identity line is `Name <email> seconds ±hhmm`. This crate owns the reusable, I/O-free part:
//! resolving a name/email with git's precedence (an override before `user.name`/`user.email` from
//! config) and formatting the line. The caller supplies any environment override (git's
//! `GIT_<role>_NAME` / `GIT_<role>_EMAIL`) and the timestamp — reading the process environment and the
//! clock is a frontend concern, kept out of the engine.

use anyhow::{Context, Result};
use gitana_config::GitConfig;

/// Build a `role` (`AUTHOR` / `COMMITTER`) identity line: the `name`/`email` override (from git's
/// `GIT_<role>_*` environment) else `user.name` / `user.email` from `config`, formatted with `when`
/// (`seconds ±hhmm`). Errors — naming the missing field — when a name or email is set nowhere, for
/// operations like `commit` that must record a real identity.
pub fn signature(
	role: &str,
	name: Option<String>,
	email: Option<String>,
	config: Option<&GitConfig>,
	when: &str,
) -> Result<String> {
	let (name, email) = resolve(name, email, config);
	let name =
		name.with_context(|| format!("identity name not set (GIT_{role}_NAME or user.name)"))?;
	let email =
		email.with_context(|| format!("identity email not set (GIT_{role}_EMAIL or user.email)"))?;
	Ok(line(&name, &email, when))
}

/// Like [`signature`], but never fails: an unset name or email falls back to a placeholder. Used for
/// reflog entries (e.g. `reset`), which git records without requiring a configured identity.
pub fn signature_or_default(
	name: Option<String>,
	email: Option<String>,
	config: Option<&GitConfig>,
	when: &str,
) -> String {
	let (name, email) = resolve(name, email, config);
	let name = name.unwrap_or_else(|| "unknown".to_owned());
	let email = email.unwrap_or_else(|| "unknown@localhost".to_owned());
	line(&name, &email, when)
}

/// Format a git identity line: `Name <email> <when>`, where `when` is `seconds ±hhmm`.
pub fn line(name: &str, email: &str, when: &str) -> String {
	format!("{name} <{email}> {when}")
}

/// Resolve `(name, email)` with git's precedence: the override (from env) else git config `user.*`.
fn resolve(
	name: Option<String>,
	email: Option<String>,
	config: Option<&GitConfig>,
) -> (Option<String>, Option<String>) {
	let from_config =
		|key: &str| config.and_then(|c| c.get_string("user", None, key).map(str::to_owned));
	let name = name.or_else(|| from_config("name"));
	let email = email.or_else(|| from_config("email"));
	(name, email)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config() -> GitConfig {
		GitConfig::parse("[user]\n\tname = Cfg Name\n\temail = cfg@example.com\n").unwrap()
	}

	#[test]
	fn formats_a_line() {
		assert_eq!(
			line("A U Thor", "a@example.com", "10 +0000"),
			"A U Thor <a@example.com> 10 +0000"
		);
	}

	#[test]
	fn env_override_takes_precedence_over_config() {
		let sig = signature(
			"AUTHOR",
			Some("Env Name".to_owned()),
			Some("env@example.com".to_owned()),
			Some(&config()),
			"10 +0000",
		)
		.unwrap();
		assert_eq!(sig, "Env Name <env@example.com> 10 +0000");
	}

	#[test]
	fn falls_back_to_config_when_no_override() {
		let sig = signature("COMMITTER", None, None, Some(&config()), "20 +0000").unwrap();
		assert_eq!(sig, "Cfg Name <cfg@example.com> 20 +0000");
	}

	#[test]
	fn errors_naming_the_missing_field() {
		let err = signature(
			"AUTHOR",
			None,
			Some("a@example.com".to_owned()),
			None,
			"0 +0000",
		)
		.unwrap_err();
		assert!(err.to_string().contains("identity name not set"), "{err}");
		assert!(err.to_string().contains("GIT_AUTHOR_NAME"), "{err}");
	}

	#[test]
	fn or_default_uses_a_placeholder() {
		assert_eq!(
			signature_or_default(None, None, None, "0 +0000"),
			"unknown <unknown@localhost> 0 +0000"
		);
	}
}
