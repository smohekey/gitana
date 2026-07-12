//! One resolved credential helper and the git wire protocol it speaks.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Result, bail};
use gitana_remote::CredentialRequest;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A single credential helper resolved from a `credential.helper` value, ready to be invoked for a
/// `get` / `store` / `erase` operation. Git turns the config value into a shell command line by three
/// forms (`credential.c` `credential_do`):
///
/// - a leading `!` → the rest is a shell command;
/// - an absolute path → run directly;
/// - otherwise → `git credential-<value>` (so `osxkeychain` runs the `git-credential-osxkeychain`
///   that lives in git's exec path, not on `PATH`).
///
/// The operation (`get`/`store`/`erase`) is appended as the final argument, and git runs the whole
/// line through the shell (`use_shell = 1`); this mirrors that with `sh -c`, so a value carrying its
/// own arguments (`foo --opt`) and shell forms both behave as git's do. Unix-oriented, like the
/// `/dev/tty` prompt fallback in [`super::super::prompt`].
pub(crate) struct Helper {
	/// The shell command line git builds for this helper, minus the trailing operation argument.
	command: String,
}

/// The credential state after a helper's `get` — the *resulting* username/password (starting from what
/// the helper was fed and mutated by its output), plus its `quit` request. This mirrors git's single
/// mutable `struct credential` threaded through the chain: a helper's output can set a field, and a
/// `url=` line resets the whole credential (clearing a field the helper does not respecify). Ancillary
/// to [`Helper::get`]. git also lets a helper refine `protocol`/`host`/`path` for later helpers; gitana
/// does not thread those (the helpers it targets — osxkeychain, store, cache, manager — return only
/// `username`/`password`/`quit`/`url` on `get`), so they are ignored.
#[derive(Debug, Default)]
pub(crate) struct GetOutput {
	pub username: Option<String>,
	pub password: Option<String>,
	/// The helper asked us to stop consulting helpers entirely (git's `quit`/`terminate`).
	pub quit: bool,
	/// The helper returned a `url=`, which in git resets the credential *and clears the remaining helper
	/// list* — so no later helper in the chain is consulted (the caller then prompts for any gap).
	pub reset: bool,
}

impl Helper {
	/// Parse a `credential.helper` config value into the shell command line git would run for it
	/// (without the trailing operation). See [`Helper`] for the three value forms.
	pub(crate) fn parse(value: &str) -> Self {
		let command = if let Some(shell) = value.strip_prefix('!') {
			shell.to_owned()
		} else if Path::new(value).is_absolute() {
			value.to_owned()
		} else {
			format!("git credential-{value}")
		};
		Self { command }
	}

	/// Run the helper's `get`, feeding it what is already known (`username`/`password` from the URL/config
	/// or an earlier helper in the chain; `wwwauth` from the `401`), and return the resulting credential
	/// state (starting from the fed-in username/password, mutated by the helper's output). A helper that
	/// cannot be spawned leaves the state unchanged (git warns and moves on); but a malformed control
	/// field in its output (a bad `quit` boolean or an unparseable `url=`) is fatal, as git aborts on it.
	/// A helper's *output* is otherwise consumed even when it then exits non-zero, as git does.
	pub(crate) async fn get(
		&self,
		request: &CredentialRequest,
		username: Option<&str>,
		password: Option<&str>,
		use_http_path: bool,
		cwd: &Path,
	) -> Result<GetOutput> {
		let input = request_lines(request, username, password, use_http_path, &request.wwwauth);
		let mut state = GetOutput {
			username: username.map(str::to_owned),
			password: password.map(str::to_owned),
			quit: false,
			reset: false,
		};
		if let Some(output) = self.run("get", &input, cwd, true).await {
			apply_get_output(&String::from_utf8_lossy(&output), &mut state)?;
		}
		Ok(state)
	}

	/// Run the helper's `store` for an accepted `credential`. Best-effort (git's `credential approve`):
	/// the outcome is never read and a failure never propagates.
	pub(crate) async fn store(
		&self,
		request: &CredentialRequest,
		username: &str,
		password: &str,
		use_http_path: bool,
		cwd: &Path,
	) {
		let input = request_lines(request, Some(username), Some(password), use_http_path, &[]);
		let _ = self.run("store", &input, cwd, false).await;
	}

	/// Run the helper's `erase` for a rejected `credential`. Best-effort (git's `credential reject`).
	pub(crate) async fn erase(
		&self,
		request: &CredentialRequest,
		username: &str,
		password: &str,
		use_http_path: bool,
		cwd: &Path,
	) {
		let input = request_lines(request, Some(username), Some(password), use_http_path, &[]);
		let _ = self.run("erase", &input, cwd, false).await;
	}

	/// Spawn the helper for `operation`, feeding `input` on stdin and (when `want_output`) capturing
	/// stdout. Runs from `cwd`, as git runs a helper from the directory it chdir'd to, so a relative
	/// or `!`-shell helper resolves paths the way git's does. Returns `Some(stdout)` once the helper has
	/// run — **including when it exits non-zero**, since git still consumes a `get` helper's output in
	/// that case — and `None` only when the helper could not be spawned/awaited at all, which the caller
	/// treats as "supplied nothing".
	async fn run(
		&self,
		operation: &str,
		input: &[u8],
		cwd: &Path,
		want_output: bool,
	) -> Option<Vec<u8>> {
		let command_line = format!("{} {operation}", self.command);
		let mut child = Command::new("sh")
			.arg("-c")
			.arg(&command_line)
			.current_dir(cwd)
			.stdin(Stdio::piped())
			.stdout(if want_output {
				Stdio::piped()
			} else {
				Stdio::null()
			})
			.spawn()
			.map_err(|error| {
				eprintln!("warning: unable to run credential helper '{command_line}': {error}");
			})
			.ok()?;

		// Feed the request on stdin; closing it (drop) signals EOF. A helper that closes its stdin early
		// (a `SIGPIPE` on our write) is not fatal — ignore the write result, as git ignores `SIGPIPE` here.
		if let Some(mut stdin) = child.stdin.take() {
			let _ = stdin.write_all(input).await;
			drop(stdin);
		}

		let output = child
			.wait_with_output()
			.await
			.map_err(|error| {
				eprintln!("warning: credential helper '{command_line}' failed: {error}");
			})
			.ok()?;
		// The exit status is not gated on: git ignores a helper's exit code (it consumes a `get`'s output
		// regardless, and `store`/`erase` are best-effort). A non-zero exit merely means no useful output.
		Some(output.stdout)
	}
}

/// Build a helper request body: `key=value\n` lines terminated by a blank line, in git's field order
/// (`credential.c` `credential_write`). `path` is emitted only under `use_http_path`; `username` and
/// `password` only when known; each `wwwauth` challenge as its own `wwwauth[]` line. Built as raw bytes
/// so a decoded path carrying a non-UTF-8 octet (`%FF` → `0xFF`) reaches the helper exactly as git
/// sends it. A value carrying a newline would corrupt the protocol, so such a field is dropped (git
/// aborts; dropping keeps the best-effort callbacks from failing an operation over a pathological URL).
fn request_lines(
	request: &CredentialRequest,
	username: Option<&str>,
	password: Option<&str>,
	use_http_path: bool,
	wwwauth: &[String],
) -> Vec<u8> {
	let mut out = Vec::new();
	let mut line = |key: &str, value: &[u8]| {
		if !value.contains(&b'\n') {
			out.extend_from_slice(key.as_bytes());
			out.push(b'=');
			out.extend_from_slice(value);
			out.push(b'\n');
		}
	};
	line("protocol", request.protocol.as_bytes());
	line("host", request.host.as_bytes());
	if use_http_path && let Some(path) = &request.path {
		// A helper receives the fully-decoded path (git's `url_decode`): `a%20b` → `a b`, `a%2Fb` → `a/b`,
		// a non-UTF-8 `%FF` → the raw byte `0xFF`. An encoded NUL stays literal (`%00`), so no `key=value`
		// line is truncated and the credential is not mis-keyed onto a shorter path.
		line("path", &gitana_remote::percent_decode_bytes(path));
	}
	if let Some(username) = username {
		line("username", username.as_bytes());
	}
	if let Some(password) = password {
		line("password", password.as_bytes());
	}
	for challenge in wwwauth {
		line("wwwauth[]", challenge.as_bytes());
	}
	out.push(b'\n');
	out
}

/// Apply a helper's `get` output — `key=value` lines up to a blank line or EOF (`credential.c`
/// `credential_read`) — to the running credential `state`, mutating it line by line as git mutates its
/// `struct credential`. Recognises the fields gitana acts on and `quit`; unknown keys are ignored, as
/// git ignores them to stay forward-compatible. A line with no `=` ends the response (git stops reading
/// there, keeping the fields already parsed); a malformed `quit` boolean or an unparseable `url=` is a
/// fatal error, as git dies on it — so a bad control field aborts the operation even when a complete
/// credential was also returned.
fn apply_get_output(output: &str, state: &mut GetOutput) -> Result<()> {
	for raw in output.lines() {
		if raw.is_empty() {
			break;
		}
		let Some((key, value)) = raw.split_once('=') else {
			// A line without `=` is malformed; git stops reading the rest of the response here.
			break;
		};
		match key {
			"username" => state.username = Some(value.to_owned()),
			"password" => state.password = Some(value.to_owned()),
			// A helper may return a whole credential as a `url=…`; git's `credential_from_url` *clears* the
			// entire credential state — repopulating username/password from the URL (a field the URL omits
			// resets to absent), and resetting `quit` too, so a `quit=1` emitted *before* the `url=` no
			// longer aborts (a `quit=` *after* it still wins, processed later in line order) — *and clears
			// the helper list*, ending the chain. The refined protocol/host/path are not threaded (see
			// [`GetOutput`]).
			"url" => {
				(state.username, state.password) = url_userinfo(value)?;
				state.quit = false;
				state.reset = true;
			}
			// git treats a malformed `quit`/`terminate` boolean as a fatal config error.
			"quit" => {
				state.quit = crate::git_config::parse_git_bool(value).ok_or_else(|| {
					anyhow::anyhow!("credential helper returned a bad boolean 'quit={value}'")
				})?
			}
			_ => {}
		}
	}
	Ok(())
}

/// Extract the `(username, password)` from a helper's `url=` (`scheme://user:pass@host/…`), each
/// percent-decoded as git decodes them (absent parts are `None`). `Err` when the value is not a URL git
/// could parse — no `scheme://`, or a decoded component (host, path, or userinfo) carries a newline or
/// carriage return — both of which git's `credential_from_url` treats as fatal (`check_url_component`).
fn url_userinfo(url: &str) -> Result<(Option<String>, Option<String>)> {
	// A malformed URL may carry a password in its userinfo, so error messages report the URL with any
	// `user:pass@` stripped — never leaking the secret to stderr or a captured log. Uses a local redactor
	// (not `anonymize_url`) because a *schemeless* value like `alice:s3cr3t@host` still carries userinfo.
	let redacted = redact_url(url);
	let Some((protocol, rest)) = url.split_once("://") else {
		bail!("credential helper returned an unparseable url '{redacted}'");
	};
	if protocol.is_empty() {
		// git rejects a URL with an empty scheme (`://host`) as unparseable, like a missing `://`.
		bail!("credential helper returned a url with no scheme '{redacted}'");
	}
	let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
	let (userinfo, host) = match rest[..authority_end].rsplit_once('@') {
		Some((userinfo, host)) => (Some(userinfo), host),
		None => (None, &rest[..authority_end]),
	};
	let decode = gitana_remote::percent_decode;
	let (username, password) = match userinfo {
		Some(userinfo) => match userinfo.split_once(':') {
			Some((user, password)) => (Some(decode(user)), Some(decode(password))),
			None => (Some(decode(userinfo)), None),
		},
		None => (None, None),
	};
	// git rejects the whole URL if any decoded component contains a control character.
	let has_control = |value: &str| value.contains(['\n', '\r']);
	if has_control(protocol)
		|| has_control(&decode(host))
		|| has_control(&decode(&rest[authority_end..]))
		|| username.as_deref().is_some_and(|user| has_control(user))
		|| password
			.as_deref()
			.is_some_and(|password| has_control(password))
	{
		bail!("credential helper returned a url with a control character '{redacted}'");
	}
	Ok((username, password))
}

/// Strip a `user[:pass]@` userinfo from `url` for safe display in an error, tolerating a malformed or
/// schemeless value (`alice:s3cr3t@host` → `host`) that `anonymize_url` would leave intact.
fn redact_url(url: &str) -> String {
	let (scheme, rest) = match url.split_once("://") {
		Some((scheme, rest)) => (format!("{scheme}://"), rest),
		None => (String::new(), url),
	};
	let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
	match rest[..authority_end].rsplit_once('@') {
		Some((_userinfo, host_port)) => format!("{scheme}{host_port}{}", &rest[authority_end..]),
		None => format!("{scheme}{rest}"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn request(path: Option<&str>) -> CredentialRequest {
		CredentialRequest {
			protocol: "https".to_owned(),
			host: "example.com".to_owned(),
			path: path.map(str::to_owned),
			username: None,
			wwwauth: vec!["Basic realm=\"x\"".to_owned()],
		}
	}

	#[test]
	fn parse_selects_the_three_command_forms() {
		assert_eq!(
			Helper::parse("osxkeychain").command,
			"git credential-osxkeychain"
		);
		assert_eq!(Helper::parse("!my helper").command, "my helper");
		assert_eq!(Helper::parse("/usr/bin/helper").command, "/usr/bin/helper");
		// A named value may carry its own arguments.
		assert_eq!(
			Helper::parse("cache --timeout=300").command,
			"git credential-cache --timeout=300"
		);
	}

	/// The request body as a UTF-8 string (the test inputs are ASCII).
	fn request_text(
		request: &CredentialRequest,
		username: Option<&str>,
		password: Option<&str>,
		use_http_path: bool,
		wwwauth: &[String],
	) -> String {
		String::from_utf8(request_lines(
			request,
			username,
			password,
			use_http_path,
			wwwauth,
		))
		.unwrap()
	}

	#[test]
	fn get_input_omits_path_unless_use_http_path_and_carries_wwwauth() {
		let without = request_text(
			&request(Some("acme/app.git")),
			Some("alice"),
			None,
			false,
			&["Basic realm=\"x\"".to_owned()],
		);
		assert_eq!(
			without,
			"protocol=https\nhost=example.com\nusername=alice\nwwwauth[]=Basic realm=\"x\"\n\n"
		);
		let with = request_text(
			&request(Some("acme/app.git")),
			Some("alice"),
			None,
			true,
			&[],
		);
		assert_eq!(
			with,
			"protocol=https\nhost=example.com\npath=acme/app.git\nusername=alice\n\n"
		);
	}

	#[test]
	fn store_input_includes_password() {
		let input = request_text(&request(None), Some("alice"), Some("s3cr3t"), false, &[]);
		assert_eq!(
			input,
			"protocol=https\nhost=example.com\nusername=alice\npassword=s3cr3t\n\n"
		);
	}

	#[test]
	fn path_line_is_fully_decoded_but_preserves_encoded_nul() {
		// A helper receives git's `url_decode`d path: `%20` → space, `%2F` → `/`, but `%00` stays literal
		// so the `key=value` line is never truncated onto a shorter path.
		let input = request_text(&request(Some("a%20b/c%2Fd/e%00f")), None, None, true, &[]);
		assert!(
			input.contains("path=a b/c/d/e%00f\n"),
			"unexpected path line in: {input:?}"
		);
	}

	#[test]
	fn path_line_preserves_a_raw_non_utf8_byte() {
		// A `%FF` decodes to the raw byte `0xFF`, sent to the helper verbatim (not a UTF-8 replacement),
		// so distinct paths stay distinct keys.
		let input = request_lines(&request(Some("a%FFb")), None, None, true, &[]);
		let needle = b"path=a\xffb\n";
		assert!(
			input.windows(needle.len()).any(|window| window == needle),
			"raw 0xFF byte not preserved in: {input:?}"
		);
	}

	/// Apply `output` to a fresh (empty) credential state — the common case in these tests.
	fn parsed(output: &str) -> GetOutput {
		let mut state = GetOutput::default();
		apply_get_output(output, &mut state).unwrap();
		state
	}

	#[test]
	fn parse_reads_fields_and_quit_stops_at_blank_line() {
		let out = parsed("username=bob\npassword=pw\nquit=1\n\nusername=ignored\n");
		assert_eq!(out.username.as_deref(), Some("bob"));
		assert_eq!(out.password.as_deref(), Some("pw"));
		assert!(out.quit);
	}

	#[test]
	fn parse_ignores_unknown_keys_but_stops_at_a_malformed_line() {
		// An unknown but well-formed `key=value` is skipped; parsing continues past it.
		let out = parsed("capability[]=authtype\nusername=bob\n");
		assert_eq!(out.username.as_deref(), Some("bob"));
		// A line with no `=` ends the response (git stops reading there): fields after it are dropped.
		let out = parsed("username=bob\ngarbage\npassword=discarded\n");
		assert_eq!(out.username.as_deref(), Some("bob"));
		assert_eq!(
			out.password, None,
			"fields after a malformed line are not read"
		);
	}

	#[test]
	fn parse_aborts_on_a_malformed_quit_even_with_a_complete_credential() {
		// git dies on a non-boolean `quit`; since `fill` checks completeness before `quit`, coercing it
		// to false/true would wrongly accept the credential — so a malformed `quit` is a hard error.
		let mut state = GetOutput::default();
		assert!(apply_get_output("username=a\npassword=b\nquit=bogus\n", &mut state).is_err());
		// An empty value is a valid git-false, not malformed.
		assert!(!parsed("quit=\n").quit);
	}

	#[test]
	fn parse_reads_a_url_response_into_username_and_password() {
		// A helper may answer with a whole credential as `url=`; git decomposes it.
		let out = parsed("url=https://alice:s3cr3t@example.com/\n");
		assert_eq!(out.username.as_deref(), Some("alice"));
		assert_eq!(out.password.as_deref(), Some("s3cr3t"));
		// Percent-escapes in the userinfo are decoded (`%40` → `@`).
		let out = parsed("url=https://alice%40org:p%20w@example.com\n");
		assert_eq!(out.username.as_deref(), Some("alice@org"));
		assert_eq!(out.password.as_deref(), Some("p w"));
	}

	#[test]
	fn parse_aborts_on_an_unparseable_url_response() {
		// git dies when a helper's `url=` cannot be parsed as a URL.
		let mut state = GetOutput::default();
		assert!(apply_get_output("url=garbage\n", &mut state).is_err());
	}

	#[test]
	fn parse_aborts_on_a_url_response_with_a_control_character() {
		// git rejects the whole URL when a decoded component carries a newline, even though the userinfo
		// alone would form a complete credential.
		let mut state = GetOutput::default();
		let error = apply_get_output("url=https://alice:s3cr3t@example.com/%0A\n", &mut state)
			.expect_err("a control character must be fatal");
		// The error must not leak the password embedded in the URL's userinfo.
		assert!(
			!format!("{error:#}").contains("s3cr3t"),
			"error leaked the userinfo password: {error:#}"
		);
	}

	#[test]
	fn a_url_response_clears_a_field_it_does_not_respecify() {
		// git's credential_from_url resets the credential: a prior password is cleared when the `url=`
		// carries no password (only a username).
		let mut state = GetOutput {
			username: None,
			password: Some("earlier".to_owned()),
			quit: false,
			reset: false,
		};
		apply_get_output("url=https://alice@example.com\n", &mut state).unwrap();
		assert_eq!(state.username.as_deref(), Some("alice"));
		assert_eq!(state.password, None, "the earlier password must be cleared");
		// A `url=` also ends the helper chain (git clears the helper list).
		assert!(state.reset);
	}

	#[test]
	fn redact_url_strips_userinfo_including_schemeless() {
		assert_eq!(
			redact_url("https://alice:s3cr3t@example.com/repo"),
			"https://example.com/repo"
		);
		// A schemeless value still has its userinfo stripped (where `anonymize_url` would not).
		assert_eq!(redact_url("alice:s3cr3t@example.com"), "example.com");
		// No userinfo — unchanged.
		assert_eq!(redact_url("garbage"), "garbage");
	}

	#[test]
	fn a_url_response_resets_quit_by_line_order() {
		// git's `credential_from_url` clears the whole state, so a `quit=1` *before* a `url=` is reset...
		assert!(!parsed("quit=1\nurl=https://alice@example.com\n").quit);
		// ...but a `quit=1` *after* the `url=` still takes effect.
		assert!(parsed("url=https://alice@example.com\nquit=1\n").quit);
	}
}
