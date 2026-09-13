//! Smoke test: `gta-mcp --mcp` speaks MCP over stdio and preserves its tools and schema resource.
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
			r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
			r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"clap://schema"}}"#,
			r#"{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"unknown://resource"}}"#,
		] {
			writeln!(stdin, "{message}").expect("write request");
		}
		// Drop stdin → EOF → the server flushes its replies and exits (no hang).
	}

	let output = child.wait_with_output().expect("wait for gta-mcp");
	let stdout = String::from_utf8_lossy(&output.stdout);
	let replies: Vec<serde_json::Value> = stdout
		.lines()
		.map(|line| serde_json::from_str(line).expect("MCP JSON reply"))
		.collect();
	let reply = |id| {
		replies
			.iter()
			.find(|reply| reply["id"].as_i64() == Some(id))
			.unwrap_or_else(|| panic!("missing reply {id}: {stdout}"))
	};
	assert!(
		reply(1)["result"]["capabilities"]["resources"].is_object(),
		"initialize must advertise resources: {}",
		reply(1)
	);

	// The tools/list reply carries each command as a tool. Spot-check read-only commands
	// and commands that need named args in the MCP surface.
	let tools = reply(2)["result"]["tools"]
		.as_array()
		.expect("tools/list array");
	for tool in [
		"status",
		"log",
		"update-ref",
		"symbolic-ref",
		"clone",
		"worktree_add",
		"submodule_status",
		"submodule_deinit",
		"submodule_set_branch",
		"remote_set_url",
	] {
		assert!(
			tools.iter().any(|candidate| candidate["name"] == tool),
			"tools/list should advertise `{tool}`; got: {}",
			reply(2)
		);
	}
	assert_eq!(
		tools
			.iter()
			.filter(|candidate| candidate["name"] == "status")
			.count(),
		1,
		"the top-level status tool must not be overwritten by submodule status"
	);
	let worktree_remove = tools
		.iter()
		.find(|candidate| candidate["name"] == "worktree_remove")
		.expect("worktree_remove tool");
	assert_eq!(
		worktree_remove["inputSchema"]["properties"]["force"]["minimum"],
		0
	);
	assert_eq!(
		worktree_remove["inputSchema"]["properties"]["force"]["maximum"],
		255
	);
	let set_branch = tools
		.iter()
		.find(|candidate| candidate["name"] == "submodule_set_branch")
		.expect("submodule_set_branch tool");
	let choices = set_branch["inputSchema"]["allOf"][0]["oneOf"]
		.as_array()
		.expect("set-branch exclusive choice schema");
	assert_eq!(choices.len(), 2);
	assert!(
		choices
			.iter()
			.any(|choice| choice["required"] == serde_json::json!(["branch"]))
	);
	assert!(choices.iter().any(|choice| {
		choice["required"] == serde_json::json!(["default"])
			&& choice["properties"]["default"]["const"] == true
	}));

	let resources = reply(3)["result"]["resources"]
		.as_array()
		.expect("resources/list array");
	assert_eq!(resources.len(), 1);
	let resource = &resources[0];
	assert_eq!(resource["uri"], "clap://schema");
	assert_eq!(resource["name"], "clap-schema");
	assert_eq!(resource["title"], "Clap CLI schema");
	assert_eq!(resource["mimeType"], "application/json");

	let contents = reply(4)["result"]["contents"]
		.as_array()
		.expect("resources/read contents");
	assert_eq!(contents.len(), 1);
	assert_eq!(contents[0]["uri"], "clap://schema");
	assert_eq!(contents[0]["mimeType"], "application/json");
	let schema: serde_json::Value =
		serde_json::from_str(contents[0]["text"].as_str().expect("schema text content"))
			.expect("valid clap schema JSON");
	assert_eq!(schema["root"]["name"], "gta-mcp");
	let root_args = schema["root"]["args"].as_array().expect("root args");
	assert!(root_args.iter().any(|argument| argument["id"] == "config"));
	assert!(!root_args.iter().any(|argument| argument["id"] == "mcp"));
	assert!(
		!root_args
			.iter()
			.any(|argument| argument["id"] == "mcp-http")
	);
	let root_commands = schema["root"]["subcommands"]
		.as_array()
		.expect("root subcommands");
	let submodule = root_commands
		.iter()
		.find(|command| command["name"] == "submodule")
		.expect("submodule command schema");
	assert!(
		submodule["subcommands"]
			.as_array()
			.expect("submodule actions")
			.iter()
			.any(|command| command["name"] == "status"),
		"resource schema must retain the hierarchical clap command name"
	);
	assert_eq!(reply(5)["error"]["code"], -32602);

	let ls_files = tools
		.iter()
		.find(|tool| tool["name"] == "ls-files")
		.expect("ls-files tool");
	assert!(
		ls_files["inputSchema"]["properties"].get("z").is_none(),
		"MCP ls-files must not advertise raw NUL-delimited output: {ls_files}"
	);
}
