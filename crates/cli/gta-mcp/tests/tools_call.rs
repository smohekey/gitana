//! Round-trip test: an MCP `tools/call` actually runs a gta command via the subprocess
//! model and returns its captured stdout. Unlike the `tools/list` smoke test, a tool call
//! spawns a subprocess, so stdin must stay open until the reply arrives — a background
//! reader thread with a receive timeout keeps the test from hanging if the server stalls.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn mcp_tools_call_runs_commands_and_escapes_positional_paths() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path().join("repo");

	let mut child = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("--mcp")
		.current_dir(dir.path())
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.expect("spawn gta-mcp --mcp");
	let mut stdin = child.stdin.take().expect("stdin");

	// Drain stdout on a thread so reads can't deadlock with our writes; each line is a reply.
	let stdout = child.stdout.take().expect("stdout");
	let (tx, rx) = mpsc::channel();
	let reader = std::thread::spawn(move || {
		let mut reader = BufReader::new(stdout);
		let mut line = String::new();
		while reader.read_line(&mut line).unwrap_or(0) > 0 {
			if tx.send(line.clone()).is_err() {
				break;
			}
			line.clear();
		}
	});

	let mut send = |message: String| {
		writeln!(stdin, "{message}").expect("write request");
		stdin.flush().expect("flush");
	};
	let recv = |rx: &mpsc::Receiver<String>| {
		rx.recv_timeout(Duration::from_secs(20))
			.expect("a reply before timeout")
	};

	send(
		r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}"#
			.to_owned(),
	);
	let init = recv(&rx);
	assert!(init.contains("\"id\":1"), "initialize reply: {init}");
	send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_owned());

	// Schema-invalid values must be rejected by the MCP boundary, never silently omitted from argv.
	// If the object-valued path were dropped, `init` would mutate the server's current directory.
	send(
		r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"init","arguments":{"path":{}}}}"#
			.to_owned(),
	);
	let reply = recv(&rx);
	assert!(reply.contains("\"id\":10"), "invalid path reply: {reply}");
	assert!(
		reply.contains("\"code\":-32602"),
		"invalid path reply: {reply}"
	);
	assert!(!dir.path().join(".git").exists());

	let redirected = dir.path().join("wrong-directory-target");
	send(format!(
		r#"{{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{{"name":"init","arguments":{{"directory":{{}},"path":"{}"}}}}}}"#,
		redirected.display()
	));
	let reply = recv(&rx);
	assert!(
		reply.contains("\"id\":11"),
		"invalid directory reply: {reply}"
	);
	assert!(
		reply.contains("\"code\":-32602"),
		"invalid directory reply: {reply}"
	);
	assert!(!redirected.exists());

	// Count arguments are bounded before synchronous argv expansion.
	send(
		r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"worktree_remove","arguments":{"path":"unused","force":256}}}"#
			.to_owned(),
	);
	let reply = recv(&rx);
	assert!(
		reply.contains("\"id\":12"),
		"excessive count reply: {reply}"
	);
	assert!(
		reply.contains("\"code\":-32602"),
		"excessive count reply: {reply}"
	);

	// Call the `init` tool (one positional arg, `path`) to create a repo in the temp dir.
	send(format!(
		r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"init","arguments":{{"path":"{}"}}}}}}"#,
		repo.display()
	));
	let reply = recv(&rx);

	assert!(reply.contains("\"id\":2"), "tools/call reply: {reply}");
	assert!(
		!reply.contains("\"isError\":true"),
		"tools/call reported an error: {reply}"
	);
	// The handler's stdout was captured into the tool result.
	assert!(
		reply.contains("Initialized empty Gitana repository"),
		"tool result should carry init's stdout: {reply}"
	);
	// And the command actually ran: the repository skeleton exists on disk.
	assert!(
		repo.join(".git/HEAD").exists(),
		"init should have created the repository at {}",
		repo.display()
	);

	std::fs::write(repo.join("-input"), b"dash-prefixed path\n").unwrap();
	let expected = Command::new("git")
		.args(["-C", repo.to_str().unwrap(), "hash-object", "--", "-input"])
		.output()
		.expect("git hash-object oracle");
	assert!(expected.status.success());
	let expected = String::from_utf8(expected.stdout).unwrap();
	send(format!(
		r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"hash-object","arguments":{{"directory":"{}","file":"-input"}}}}}}"#,
		repo.display()
	));
	let reply = recv(&rx);
	assert!(reply.contains("\"id\":3"), "tools/call reply: {reply}");
	assert!(
		!reply.contains("\"isError\":true"),
		"dash-prefixed positional was treated as an option: {reply}"
	);
	assert!(
		reply.contains(expected.trim()),
		"hash-object result should match git: {reply}"
	);

	Command::new("git")
		.args([
			"-C",
			repo.to_str().unwrap(),
			"config",
			"user.name",
			"MCP Test",
		])
		.status()
		.unwrap();
	Command::new("git")
		.args([
			"-C",
			repo.to_str().unwrap(),
			"config",
			"user.email",
			"mcp@example.com",
		])
		.status()
		.unwrap();
	std::fs::write(repo.join("tracked"), b"content\n").unwrap();
	assert!(
		Command::new("git")
			.args(["-C", repo.to_str().unwrap(), "add", "tracked"])
			.status()
			.unwrap()
			.success()
	);
	send(format!(
		r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"commit","arguments":{{"directory":"{}","message":"-hello"}}}}}}"#,
		repo.display()
	));
	let reply = recv(&rx);
	assert!(reply.contains("\"id\":4"), "commit reply: {reply}");
	assert!(
		!reply.contains("\"isError\":true"),
		"dash-prefixed option value was treated as an option: {reply}"
	);
	let subject = Command::new("git")
		.args(["-C", repo.to_str().unwrap(), "log", "-1", "--format=%s"])
		.output()
		.unwrap();
	assert!(subject.status.success());
	assert_eq!(String::from_utf8(subject.stdout).unwrap().trim(), "-hello");

	std::fs::write(repo.join("tracked"), b"worktree change\n").unwrap();
	send(format!(
		r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"status","arguments":{{"directory":"{}"}}}}}}"#,
		repo.display()
	));
	let reply = recv(&rx);
	assert!(reply.contains("\"id\":5"), "status reply: {reply}");
	assert!(
		reply.contains(r#""text":" M tracked"#),
		"the worktree-only status sigil must retain its leading blank: {reply}"
	);

	drop(stdin);
	let _ = child.wait();
	let _ = reader.join();
}
