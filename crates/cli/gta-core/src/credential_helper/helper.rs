//! One resolved credential helper and the git wire protocol it speaks.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Result, bail};
use gitana_remote::{Credential, CredentialRequest};
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
	/// A pre-encoded scheme the helper returned under the `authtype` capability — git's `authtype`. When
	/// set together with [`credential`](Self::credential), the resolved credential is encoded
	/// (Bearer/Digest/…) and username/password are not used for the header (but git still keeps them).
	pub authtype: Option<String>,
	/// The pre-encoded credential value paired with [`authtype`](Self::authtype) — git's `credential`.
	pub credential: Option<String>,
	/// The helper marked the credential short-lived (git's `ephemeral`) — do not persist it.
	pub ephemeral: bool,
	/// Opaque `state[]` the helper returned under the `state` capability, to echo back next round.
	pub state: Vec<String>,
	/// The helper expects another multistage round (git's `continue`) — the credential is a non-final step.
	pub more: bool,
	/// The helper asked us to stop consulting helpers entirely (git's `quit`/`terminate`).
	pub quit: bool,
	/// The helper returned a `url=`, which in git resets the credential *and clears the remaining helper
	/// list* — so no later helper in the chain is consulted (the caller then prompts for any gap).
	pub reset: bool,
	/// Whether the helper echoed the `authtype` capability — gates honouring `authtype`/`credential`/
	/// `ephemeral` (git only uses them when both sides advertised the capability).
	pub caps_authtype: bool,
	/// Whether the helper echoed the `state` capability — gates honouring `state[]`/`continue`.
	pub caps_state: bool,
}

impl GetOutput {
	/// Whether the helper supplied a complete pre-encoded credential (`authtype`+`credential`) under the
	/// mutually-advertised `authtype` capability — git honours the encoded form only then. Any
	/// username/password the helper also set ride the resolved credential too (git keeps every populated
	/// attribute for `store`/`erase`).
	pub fn has_encoded(&self) -> bool {
		self.caps_authtype && self.authtype.is_some() && self.credential.is_some()
	}
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

	/// Run the helper's `get`, feeding it the credential state known so far (`running` — the
	/// username/password/`authtype`/`credential`/`state[]` accumulated by the URL/config and earlier
	/// helpers, plus the `401`'s `wwwauth`), and return the resulting state (starting from `running`'s
	/// credential fields and capabilities, mutated by the helper's output — git threads one mutable
	/// credential through the chain). A helper that cannot be spawned leaves the state unchanged (git warns
	/// and moves on); but a malformed control field (a bad `quit` boolean or an unparseable `url=`) is
	/// fatal, as git aborts on it. A helper's *output* is otherwise consumed even when it exits non-zero.
	pub(crate) async fn get(
		&self,
		request: &CredentialRequest,
		running: &GetOutput,
		use_http_path: bool,
		cwd: &Path,
	) -> Result<GetOutput> {
		let input = get_request_lines(request, running, use_http_path);
		// Seed from the running credential fields — the fresh `state[]` collected so far and `more` carry
		// through the chain (git retains `continue` unless a later helper sets `continue=0`), while the
		// per-invocation `quit`/`reset` start fresh. The helper's output then mutates this, so a field an
		// earlier helper set survives.
		let mut state = GetOutput {
			username: running.username.clone(),
			password: running.password.clone(),
			authtype: running.authtype.clone(),
			credential: running.credential.clone(),
			ephemeral: running.ephemeral,
			state: running.state.clone(),
			more: running.more,
			caps_authtype: running.caps_authtype,
			caps_state: running.caps_state,
			..GetOutput::default()
		};
		if let Some(output) = self.run("get", &input, cwd, true).await {
			// git treats credential values (username/password/credential/state[]) as opaque bytes; gitana
			// models them as UTF-8 `String`, so a non-UTF-8 value would be lossily decoded here. Deliberate,
			// model-wide simplification — real helper values are ASCII/UTF-8 (Basic creds, Bearer tokens,
			// base64 `state[]`). A byte-faithful model is a deferred follow-up (see docs/hlds/http-credentials).
			apply_get_output(&String::from_utf8_lossy(&output), &mut state)?;
		}
		Ok(state)
	}

	/// Run the helper's `store` for an accepted `credential`. Best-effort (git's `credential approve`):
	/// the outcome is never read and a failure never propagates.
	pub(crate) async fn store(
		&self,
		request: &CredentialRequest,
		credential: &Credential,
		use_http_path: bool,
		cwd: &Path,
	) {
		let input = credential_lines(request, credential, use_http_path);
		let _ = self.run("store", &input, cwd, false).await;
	}

	/// Run the helper's `erase` for a rejected `credential`. Best-effort (git's `credential reject`).
	pub(crate) async fn erase(
		&self,
		request: &CredentialRequest,
		credential: &Credential,
		use_http_path: bool,
		cwd: &Path,
	) {
		let input = credential_lines(request, credential, use_http_path);
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

/// Write a `key=value\n` helper-protocol line to `out`, unless `value` carries a newline (which would
/// corrupt the protocol — git aborts; dropping keeps the best-effort callbacks from failing over a
/// pathological URL). Raw bytes so a decoded path with a non-UTF-8 octet (`%FF` → `0xFF`) is sent
/// exactly as git sends it.
fn line(out: &mut Vec<u8>, key: &str, value: &[u8]) {
	if !value.contains(&b'\n') {
		out.extend_from_slice(key.as_bytes());
		out.push(b'=');
		out.extend_from_slice(value);
		out.push(b'\n');
	}
}

/// The `protocol`/`host`/`path` lines every request carries (`credential.c` `credential_write`). `path`
/// is emitted only under `use_http_path`, fully decoded (git's `url_decode`): `a%20b` → `a b`, `a%2Fb`
/// → `a/b`, a non-UTF-8 `%FF` → the raw byte, an encoded NUL kept literal so no line is truncated.
fn location_lines(out: &mut Vec<u8>, request: &CredentialRequest, use_http_path: bool) {
	line(out, "protocol", request.protocol.as_bytes());
	line(out, "host", request.host.as_bytes());
	if use_http_path && let Some(path) = &request.path {
		line(out, "path", &gitana_remote::percent_decode_bytes(path));
	}
}

/// Build a helper `get` request body — the capabilities gitana understands announced **first** (git
/// requires a `capability[]` to precede any value depending on it), then the location, the credential
/// fields known so far (`running`: any username/password or `authtype`/`credential` an earlier helper in
/// the chain supplied — git threads one mutable credential through the chain), the `401`'s `wwwauth[]`
/// challenges, and the running `state[]` (seeded from the prior multistage round). Terminated by a blank
/// line.
fn get_request_lines(
	request: &CredentialRequest,
	running: &GetOutput,
	use_http_path: bool,
) -> Vec<u8> {
	let mut out = Vec::new();
	line(&mut out, "capability[]", b"authtype");
	line(&mut out, "capability[]", b"state");
	location_lines(&mut out, request, use_http_path);
	if let Some(username) = &running.username {
		line(&mut out, "username", username.as_bytes());
	}
	if let Some(password) = &running.password {
		line(&mut out, "password", password.as_bytes());
	}
	if let Some(authtype) = &running.authtype {
		line(&mut out, "authtype", authtype.as_bytes());
	}
	if let Some(credential) = &running.credential {
		line(&mut out, "credential", credential.as_bytes());
	}
	// Forward an active `ephemeral` from an earlier helper's partial credential so a later helper sees it
	// (git writes the running credential's fields).
	if running.ephemeral {
		line(&mut out, "ephemeral", b"1");
	}
	for challenge in &request.wwwauth {
		line(&mut out, "wwwauth[]", challenge.as_bytes());
	}
	// Forward a running `continue` (git's `c->multistage`): `credential_write` emits `continue=1` under the
	// state capability when an earlier helper in the chain returned a partial, non-final credential, so the
	// next helper sees the same in-progress negotiation and finishes it rather than starting over. (git-
	// credential(5) marks `continue` one-way helper→Git for the *value's* meaning — it is not surfaced to the
	// caller — but git still threads it across the helper chain, exactly as it threads username/authtype/state.)
	if running.more {
		line(&mut out, "continue", b"1");
	}
	// git sends the *incoming* (prior round's) state unchanged to every helper — separate from the fresh
	// state a round collects — so feed `request.state`, not the accumulator's collected `running.state`.
	for state in &request.state {
		line(&mut out, "state[]", state.as_bytes());
	}
	out.push(b'\n');
	out
}

/// Build a helper `store`/`erase` request body for `credential`: the `authtype` capability, the
/// location, and every populated credential attribute — `username`/`password` and/or
/// `authtype`/`credential` (with `ephemeral` when set) — matching what git's `credential_write` hands a
/// helper to persist or erase (git writes each field it has, so username/password ride an encoded
/// credential too, keying an account-based helper). Terminated by a blank line.
fn credential_lines(
	request: &CredentialRequest,
	credential: &Credential,
	use_http_path: bool,
) -> Vec<u8> {
	let mut out = Vec::new();
	line(&mut out, "capability[]", b"authtype");
	line(&mut out, "capability[]", b"state");
	location_lines(&mut out, request, use_http_path);
	if let Some(username) = &credential.username {
		line(&mut out, "username", username.as_bytes());
	}
	if let Some(password) = &credential.password {
		line(&mut out, "password", password.as_bytes());
	}
	if let Some(authtype) = &credential.authtype {
		line(&mut out, "authtype", authtype.as_bytes());
	}
	if let Some(value) = &credential.credential {
		line(&mut out, "credential", value.as_bytes());
	}
	if credential.ephemeral {
		line(&mut out, "ephemeral", b"1");
	}
	// Forward the final round's `state[]` (git hands it to `store`/`erase` so a stateful helper can persist
	// or clean up the negotiated credential).
	for state in &request.state {
		line(&mut out, "state[]", state.as_bytes());
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
			// The helper echoes the capabilities it supports; only under a mutually-advertised capability
			// are its `authtype`/`credential`/`ephemeral` (authtype) and `state[]`/`continue` (state) honoured.
			// A `url=` reset zeroes git's *initial* advertisement (`credential_from_url` → `credential_clear`
			// → `credential_init`); a later `capability[]` echo sets only the helper-side bit, and git's
			// `OP_RESPONSE` check needs both, so a capability re-advertised *after* a `url=` in the same
			// response is never honoured — skip it once a reset has occurred.
			"capability[]" if !state.reset => match value {
				"authtype" => state.caps_authtype = true,
				"state" => state.caps_state = true,
				_ => {}
			},
			"authtype" => state.authtype = Some(value.to_owned()),
			"credential" => state.credential = Some(value.to_owned()),
			"ephemeral" => {
				state.ephemeral = crate::git_config::parse_git_bool(value).ok_or_else(|| {
					anyhow::anyhow!("credential helper returned a bad boolean 'ephemeral={value}'")
				})?
			}
			"state[]" => state.state.push(value.to_owned()),
			"continue" => {
				state.more = crate::git_config::parse_git_bool(value).ok_or_else(|| {
					anyhow::anyhow!("credential helper returned a bad boolean 'continue={value}'")
				})?
			}
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
				// `credential_from_url` clears the *entire* credential, so a `url=` after any encoded /
				// capability / ephemeral / state fields drops them too (a later `authtype=` could re-set them).
				state.authtype = None;
				state.credential = None;
				state.ephemeral = false;
				state.state.clear();
				state.more = false;
				state.caps_authtype = false;
				state.caps_state = false;
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
			carried_username: None,
			wwwauth: vec!["Basic realm=\"x\"".to_owned()],
			state: Vec::new(),
			authtype: None,
			ephemeral: false,
			caps_authtype: false,
			caps_state: false,
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

	/// A request with `path` and an explicit `wwwauth`/`state` (the default `request()` carries a Basic
	/// challenge and no state).
	fn request_with(path: Option<&str>, wwwauth: &[&str], state: &[&str]) -> CredentialRequest {
		CredentialRequest {
			wwwauth: wwwauth.iter().map(|s| (*s).to_owned()).collect(),
			state: state.iter().map(|s| (*s).to_owned()).collect(),
			..request(path)
		}
	}

	/// A `get` request body as a UTF-8 string (the test inputs are ASCII). Seeds the running credential
	/// with `username`/`password` and the request's `state[]`.
	fn get_text(
		request: &CredentialRequest,
		username: Option<&str>,
		password: Option<&str>,
		use_http_path: bool,
	) -> String {
		let running = GetOutput {
			username: username.map(str::to_owned),
			password: password.map(str::to_owned),
			state: request.state.clone(),
			..GetOutput::default()
		};
		String::from_utf8(get_request_lines(request, &running, use_http_path)).unwrap()
	}

	#[test]
	fn get_input_advertises_capabilities_and_carries_challenge_and_state() {
		let without = get_text(
			&request_with(Some("acme/app.git"), &["Basic realm=\"x\""], &["helper:s1"]),
			Some("alice"),
			None,
			false,
		);
		assert_eq!(
			without,
			"capability[]=authtype\ncapability[]=state\nprotocol=https\nhost=example.com\n\
			 username=alice\nwwwauth[]=Basic realm=\"x\"\nstate[]=helper:s1\n\n"
		);
		let with = get_text(
			&request_with(Some("acme/app.git"), &[], &[]),
			Some("alice"),
			None,
			true,
		);
		assert_eq!(
			with,
			"capability[]=authtype\ncapability[]=state\nprotocol=https\nhost=example.com\n\
			 path=acme/app.git\nusername=alice\n\n"
		);
	}

	#[test]
	fn get_input_forwards_running_continue_to_the_next_helper() {
		// An earlier helper in the chain returned a partial, non-final credential (`continue=1` under the
		// state capability). git's `credential_write` threads that `continue=1` to the next helper so it
		// finishes the same negotiation rather than starting over. `ephemeral`/`continue` ride the running
		// credential; `state[]` still comes from the incoming (prior round's) request state.
		let running = GetOutput {
			username: Some("alice".to_owned()),
			ephemeral: true,
			more: true,
			state: vec!["helper:s1".to_owned()],
			caps_authtype: true,
			caps_state: true,
			..GetOutput::default()
		};
		let request = request_with(None, &[], &["helper:s1"]);
		let text = String::from_utf8(get_request_lines(&request, &running, false)).unwrap();
		assert_eq!(
			text,
			"capability[]=authtype\ncapability[]=state\nprotocol=https\nhost=example.com\n\
			 username=alice\nephemeral=1\ncontinue=1\nstate[]=helper:s1\n\n"
		);
	}

	#[test]
	fn store_input_includes_the_credential_attributes() {
		// A Basic credential sends username/password; an encoded one sends authtype/credential (+ephemeral).
		let basic = String::from_utf8(credential_lines(
			&request(None),
			&Credential::basic("alice".to_owned(), "s3cr3t".to_owned()),
			false,
		))
		.unwrap();
		assert_eq!(
			basic,
			"capability[]=authtype\ncapability[]=state\nprotocol=https\nhost=example.com\nusername=alice\npassword=s3cr3t\n\n"
		);
		let encoded = String::from_utf8(credential_lines(
			&request(None),
			&Credential {
				username: Some("alice".to_owned()),
				authtype: Some("Bearer".to_owned()),
				credential: Some("tok".to_owned()),
				ephemeral: true,
				..Credential::default()
			},
			false,
		))
		.unwrap();
		// git keeps the resolved account name alongside the encoded credential, so `store`/`erase` carry
		// `username` before `authtype`/`credential` (an account-keyed helper keys on it).
		assert_eq!(
			encoded,
			"capability[]=authtype\ncapability[]=state\nprotocol=https\nhost=example.com\nusername=alice\nauthtype=Bearer\ncredential=tok\n\
			 ephemeral=1\n\n"
		);
	}

	#[test]
	fn path_line_is_fully_decoded_but_preserves_encoded_nul() {
		// A helper receives git's `url_decode`d path: `%20` → space, `%2F` → `/`, but `%00` stays literal
		// so the `key=value` line is never truncated onto a shorter path.
		let input = get_text(&request(Some("a%20b/c%2Fd/e%00f")), None, None, true);
		assert!(
			input.contains("path=a b/c/d/e%00f\n"),
			"unexpected path line in: {input:?}"
		);
	}

	#[test]
	fn path_line_preserves_a_raw_non_utf8_byte() {
		// A `%FF` decodes to the raw byte `0xFF`, sent to the helper verbatim (not a UTF-8 replacement),
		// so distinct paths stay distinct keys.
		let input = get_request_lines(&request(Some("a%FFb")), &GetOutput::default(), true);
		let needle = b"path=a\xffb\n";
		assert!(
			input.windows(needle.len()).any(|window| window == needle),
			"raw 0xFF byte not preserved in: {input:?}"
		);
	}

	#[test]
	fn get_input_feeds_forward_a_running_encoded_field() {
		// git threads one mutable credential: an `authtype` an earlier helper set is fed to the next.
		let running = GetOutput {
			authtype: Some("bearer".to_owned()),
			credential: Some("tok".to_owned()),
			..GetOutput::default()
		};
		let input = String::from_utf8(get_request_lines(&request(None), &running, false)).unwrap();
		assert!(
			input.contains("authtype=bearer\ncredential=tok\n"),
			"running encoded fields not fed forward: {input:?}"
		);
	}

	#[test]
	fn url_reset_clears_encoded_and_state_fields() {
		// git's `credential_from_url` clears the whole credential, so a `url=` after encoded/state fields
		// drops them (only the URL's username survives here).
		let mut state = GetOutput {
			authtype: Some("bearer".to_owned()),
			credential: Some("tok".to_owned()),
			caps_authtype: true,
			ephemeral: true,
			state: vec!["s1".to_owned()],
			more: true,
			caps_state: true,
			..GetOutput::default()
		};
		apply_get_output("url=https://alice@example.com\n", &mut state).unwrap();
		assert_eq!(state.username.as_deref(), Some("alice"));
		assert!(!state.has_encoded(), "encoded fields survived a url= reset");
		assert!(
			state.state.is_empty() && !state.more && !state.caps_authtype && !state.caps_state,
			"state/capability fields survived a url= reset"
		);
	}

	/// Apply `output` to a fresh (empty) credential state — the common case in these tests.
	fn parsed(output: &str) -> GetOutput {
		let mut state = GetOutput::default();
		apply_get_output(output, &mut state).unwrap();
		state
	}

	#[test]
	fn a_capability_re_advertised_after_a_url_reset_is_not_honoured() {
		// git's `url=` reset zeroes its initial capability advertisement, so a `capability[]` the helper
		// re-emits afterwards sets only the helper-side bit — git's `OP_RESPONSE` needs both, so a following
		// encoded credential / `state[]`+`continue` is ignored. The capability must not be re-enabled.
		let out = parsed(
			"url=https://example.com\ncapability[]=authtype\ncapability[]=state\n\
			 authtype=bearer\ncredential=tok\nstate[]=s1\ncontinue=1\n",
		);
		assert!(
			!out.caps_authtype,
			"authtype capability re-enabled after url="
		);
		assert!(!out.caps_state, "state capability re-enabled after url=");
		assert!(
			!out.has_encoded(),
			"encoded credential honoured after a url= reset"
		);
		// A capability advertised *before* the `url=` is likewise wiped by the reset (unchanged behaviour).
		let before =
			parsed("capability[]=authtype\nurl=https://example.com\nauthtype=bearer\ncredential=tok\n");
		assert!(!before.has_encoded());
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
			password: Some("earlier".to_owned()),
			..GetOutput::default()
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
