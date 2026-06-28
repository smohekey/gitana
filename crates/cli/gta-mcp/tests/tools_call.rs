//! Round-trip test: an MCP `tools/call` actually runs a gta command via the subprocess
//! model and returns its captured stdout. Unlike the `tools/list` smoke test, a tool call
//! spawns a subprocess, so stdin must stay open until the reply arrives — a background
//! reader thread with a receive timeout keeps the test from hanging if the server stalls.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn mcp_tools_call_runs_init_and_returns_stdout() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path().join("repo");

	let mut child = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("--mcp")
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

	drop(stdin);
	let _ = child.wait();
	let _ = reader.join();
}
