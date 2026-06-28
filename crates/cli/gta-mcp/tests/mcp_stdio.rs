//! Smoke test: `gta-mcp --mcp` speaks MCP over stdio and lists the gta commands as tools.
//!
//! Drives a minimal handshake (`initialize` → `initialized` → `tools/list`), then closes
//! stdin so the server exits, and asserts the expected tools are advertised — including the
//! commands that take two positional args on the `gta` CLI and are exposed with named args
//! here (`update-ref`, `symbolic-ref`, `clone`, …).

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn mcp_stdio_advertises_gta_tools() {
	let mut child = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("--mcp")
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.expect("spawn gta-mcp --mcp");

	{
		let mut stdin = child.stdin.take().expect("stdin");
		for message in [
			r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}"#,
			r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
			r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
		] {
			writeln!(stdin, "{message}").expect("write request");
		}
		// Drop stdin → EOF → the server flushes its replies and exits (no hang).
	}

	let output = child.wait_with_output().expect("wait for gta-mcp");
	let stdout = String::from_utf8_lossy(&output.stdout);

	// The tools/list reply carries each command as a tool. Spot-check read-only commands
	// and commands that need named args in the MCP surface.
	for tool in ["status", "log", "update-ref", "symbolic-ref", "clone"] {
		assert!(
			stdout.contains(&format!("\"name\":\"{tool}\"")),
			"tools/list should advertise `{tool}`; got: {stdout}"
		);
	}
}
