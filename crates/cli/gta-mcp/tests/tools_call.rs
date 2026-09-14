//! Round-trip test: an MCP `tools/call` actually runs a gta command via the subprocess
//! model and returns its captured stdout. Unlike the `tools/list` smoke test, a tool call
//! spawns a subprocess, so stdin must stay open until the reply arrives — a background
//! reader thread with a receive timeout keeps the test from hanging if the server stalls.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

#[test]
fn mcp_tools_call_runs_commands_and_escapes_positional_paths() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path().join("repo");
	let reply = call_tool(None, "init", json!({ "path": repo }));

	assert!(
		reply["result"]["isError"] != true,
		"tools/call reported an error: {reply}"
	);
	// The handler's stdout was captured into the tool result.
	assert!(
		tool_text(&reply).contains("Initialized empty Gitana repository"),
		"tool result should carry init's stdout: {reply}"
	);
	// And the command actually ran: the repository skeleton exists on disk.
	assert!(
		repo.join(".git/HEAD").exists(),
		"init should have created the repository at {}",
		repo.display()
	);
}

#[test]
fn direct_ls_files_z_preserves_exact_path_bytes() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	init_repo_with_raw_index_path(repo, b"bad-\xff");

	let output = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("-C")
		.arg(repo)
		.args(["ls-files", "-z"])
		.output()
		.expect("run direct gta-mcp ls-files -z");
	assert!(
		output.status.success(),
		"direct gta-mcp ls-files -z failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(output.stdout, b"bad-\xff\0");
}

#[test]
fn mcp_ls_files_forces_reversible_quoting() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	init_repo_with_raw_index_path(repo, b"bad-\xff");
	run_git(repo, &["config", "core.quotePath", "false"], None);

	let reply = call_tool(Some(repo), "ls-files", json!({}));
	assert!(
		reply["result"]["isError"] != true,
		"tools/call reported an error: {reply}"
	);
	assert_eq!(tool_text(&reply), "\"bad-\\377\"");
}

#[test]
fn mcp_status_forces_reversible_quoting() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	init_repo_with_raw_index_path(repo, b"bad-\xff");
	run_git(repo, &["config", "core.quotePath", "false"], None);

	let reply = call_tool(Some(repo), "status", json!({}));
	assert!(
		reply["result"]["isError"] != true,
		"tools/call reported an error: {reply}"
	);
	assert_eq!(tool_text(&reply), "AD \"bad-\\377\"");
}

#[test]
fn mcp_path_errors_preserve_exact_pathspec_identity() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	run_git(repo, &["init", "-q"], None);
	let raw = b"raw-\xff".as_slice();
	let literal_collision = b"\"raw-\\377\"".as_slice();

	let raw_reply = call_tool(
		Some(repo),
		"rm",
		json!({ "cached": true, "pathspecs": [path_token(raw)] }),
	);
	let literal_reply = call_tool(
		Some(repo),
		"rm",
		json!({ "cached": true, "pathspecs": [path_token(literal_collision)] }),
	);
	assert_eq!(raw_reply["result"]["isError"], true, "{raw_reply}");
	assert_eq!(literal_reply["result"]["isError"], true, "{literal_reply}");
	let raw_text = tool_text(&raw_reply);
	let literal_text = tool_text(&literal_reply);
	assert_eq!(
		raw_text,
		"Tool process exited with non-zero status (1)\nstderr:\ngta-mcp: pathspec did not match any file(s): \"raw-\\377\""
	);
	assert_eq!(
		literal_text,
		"Tool process exited with non-zero status (1)\nstderr:\ngta-mcp: pathspec did not match any file(s): \"\\\"raw-\\\\377\\\"\""
	);
	assert_ne!(raw_text, literal_text);
}

#[cfg(unix)]
#[test]
fn mcp_rm_result_paths_are_reversibly_quoted() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	let raw = b"bad-\xff".as_slice();
	let literal_collision = b"\"bad-\\377\"".as_slice();
	init_repo_with_index_paths(repo, &[raw, literal_collision]);

	let reply = call_tool(
		Some(repo),
		"rm",
		json!({
			"cached": true,
			"force": true,
			"dry_run": true,
			"pathspecs": [path_token(raw), path_token(literal_collision)],
		}),
	);
	assert!(
		reply["result"]["isError"] != true,
		"tools/call reported an error: {reply}"
	);
	let lines = tool_text(&reply).lines().collect::<Vec<_>>();
	let expected = [
		"rm '\"bad-\\377\"'".to_owned(),
		"rm '\"\\\"bad-\\\\377\\\"\"'".to_owned(),
	];
	assert_ne!(expected[0], expected[1]);
	assert_eq!(lines.len(), expected.len());
	for expected in &expected {
		assert!(
			lines.contains(&expected.as_str()),
			"missing {expected:?}: {reply}"
		);
	}
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn mcp_add_advisory_paths_are_reversibly_quoted() {
	use std::{ffi::OsString, os::unix::ffi::OsStringExt};

	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	run_git(repo, &["init", "-q"], None);
	std::fs::write(repo.join(".gitignore"), b"*\n").expect("write ignore rule");
	let raw = b"raw-\xff".as_slice();
	let literal_collision = b"\"raw-\\377\"".as_slice();
	for path in [raw, literal_collision] {
		std::fs::write(repo.join(OsString::from_vec(path.to_vec())), b"ignored\n")
			.expect("write ignored path");
	}
	let mut literal_collision_pathspec = b":(literal)".to_vec();
	literal_collision_pathspec.extend_from_slice(literal_collision);

	let reply = call_tool(
		Some(repo),
		"add",
		json!({
			"pathspecs": [path_token(raw), path_token(&literal_collision_pathspec)],
		}),
	);
	assert_eq!(
		reply["result"]["isError"], true,
		"expected advisory: {reply}"
	);
	let text = tool_text(&reply);
	let raw_rendered = "\"raw-\\377\"";
	let literal_rendered = "\"\\\"raw-\\\\377\\\"\"";
	assert_ne!(raw_rendered, literal_rendered);
	assert!(text.lines().any(|line| line == raw_rendered), "{reply}");
	assert!(text.lines().any(|line| line == literal_rendered), "{reply}");
}

#[test]
fn mcp_mv_result_paths_force_non_ascii_quoting() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	let source = "café";
	init_repo_with_index_paths(repo, &[source.as_bytes()]);
	std::fs::write(repo.join(source), b"content\n").expect("write UTF-8 source path");

	let reply = call_tool(
		Some(repo),
		"mv",
		json!({
			"force": true,
			"dry_run": true,
			"verbose": true,
			"paths": [source, "moved"],
		}),
	);
	assert!(
		reply["result"]["isError"] != true,
		"tools/call reported an error: {reply}"
	);
	assert_eq!(tool_text(&reply), "Renaming \"caf\\303\\251\" to moved");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn mcp_mv_result_paths_are_reversibly_quoted() {
	use std::{ffi::OsString, os::unix::ffi::OsStringExt};

	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	let raw = b"bad-\xff".as_slice();
	let literal_collision = b"\"bad-\\377\"".as_slice();
	init_repo_with_index_paths(repo, &[raw]);
	std::fs::write(repo.join(OsString::from_vec(raw.to_vec())), b"content\n")
		.expect("write raw source path");

	let reply = call_tool(
		Some(repo),
		"mv",
		json!({
			"force": true,
			"dry_run": true,
			"verbose": true,
			"paths": [path_token(raw), path_token(literal_collision)],
		}),
	);
	assert!(
		reply["result"]["isError"] != true,
		"tools/call reported an error: {reply}"
	);
	let from = "\"bad-\\377\"";
	let to = "\"\\\"bad-\\\\377\\\"\"";
	assert_ne!(from, to);
	assert_eq!(tool_text(&reply), format!("Renaming {from} to {to}"));
}

#[test]
fn gta_mcp_cat_file_forces_reversible_quoting() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	init_repo_with_raw_index_path(repo, b"bad-\xff");
	run_git(repo, &["config", "core.quotePath", "false"], None);
	let tree = run_git(repo, &["write-tree"], None);
	let tree = String::from_utf8(tree).unwrap().trim().to_owned();

	let output = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.current_dir(repo)
		.args(["cat-file", "-p", &tree])
		.output()
		.expect("run direct gta-mcp cat-file");
	assert!(
		output.status.success(),
		"gta-mcp cat-file failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(
		String::from_utf8(output.stdout)
			.unwrap()
			.contains("\"bad-\\377\""),
		"gta-mcp output must retain a reversible path"
	);
}

#[test]
fn gta_mcp_sparse_warnings_force_reversible_quoting() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	run_git(repo, &["init", "-q"], None);
	run_git(repo, &["config", "user.name", "T"], None);
	run_git(repo, &["config", "user.email", "t@example.com"], None);
	std::fs::create_dir(repo.join("café")).expect("create tracked directory");
	std::fs::write(repo.join("café/file"), b"base\n").expect("write tracked file");
	run_git(repo, &["add", "-A"], None);
	run_git(repo, &["commit", "-qm", "base"], None);
	std::fs::write(repo.join("café/file"), b"dirty\n").expect("modify tracked file");

	let output = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("-C")
		.arg(repo)
		.args(["sparse-checkout", "set"])
		.output()
		.expect("run direct gta-mcp sparse-checkout set");
	assert!(
		output.status.success(),
		"gta-mcp sparse-checkout failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(
		String::from_utf8(output.stderr).unwrap(),
		"warning: '\"caf\\303\\251/file\"' is not up to date and was left despite sparse patterns\n"
	);
}

#[test]
fn gta_mcp_sparse_list_forces_reversible_quoting() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	run_git(repo, &["init", "-q"], None);
	let raw_pattern = b"raw-\xff/**";

	let set = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("-C")
		.arg(repo)
		.args(["sparse-checkout", "set", "--no-cone", "--pattern"])
		.arg(path_token(raw_pattern))
		.output()
		.expect("set a base64-tagged raw sparse pattern");
	assert!(
		set.status.success(),
		"gta-mcp sparse-checkout set failed: {}",
		String::from_utf8_lossy(&set.stderr)
	);
	assert_eq!(
		std::fs::read(repo.join(".git/info/sparse-checkout")).unwrap(),
		b"raw-\xff/**\n",
		"machine rendering must not change the persisted sparse pattern"
	);

	let listed = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("-C")
		.arg(repo)
		.args(["sparse-checkout", "list"])
		.output()
		.expect("list a raw sparse pattern through gta-mcp");
	assert!(
		listed.status.success(),
		"gta-mcp sparse-checkout list failed: {}",
		String::from_utf8_lossy(&listed.stderr)
	);
	assert_eq!(
		String::from_utf8(listed.stdout).expect("MCP-facing sparse output must be UTF-8"),
		"\"raw-\\377/**\"\n"
	);
}

#[test]
fn gta_mcp_sparse_cone_errors_force_reversible_quoting() {
	let pattern_dir = tempfile::tempdir().expect("temp dir");
	let pattern_repo = pattern_dir.path();
	run_git(pattern_repo, &["init", "-q"], None);
	let raw_pattern = sparse_set_error(pattern_repo, b"raw-\xff*");
	let literal_pattern = sparse_set_error(pattern_repo, b"\"raw-\\377*\"");
	assert_eq!(
		raw_pattern,
		"gta-mcp: '\"raw-\\377*\"' contains a pattern character; cone directories must be literal paths\n"
	);
	assert_eq!(
		literal_pattern,
		"gta-mcp: '\"\\\"raw-\\\\377*\\\"\"' contains a pattern character; cone directories must be literal paths\n"
	);
	assert_ne!(raw_pattern, literal_pattern);

	let tracked_dir = tempfile::tempdir().expect("temp dir");
	let tracked_repo = tracked_dir.path();
	let raw_tracked = b"raw-\xff".as_slice();
	init_repo_with_raw_index_path(tracked_repo, raw_tracked);
	let raw_tracked = sparse_set_error(tracked_repo, raw_tracked);
	assert_eq!(
		raw_tracked,
		"gta-mcp: '\"raw-\\377\"' is a tracked file, not a directory\n"
	);
}

#[test]
fn mcp_show_tree_forces_reversible_quoting() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	init_repo_with_raw_index_path(repo, b"bad-\xff");
	let tree = run_git(repo, &["write-tree"], None);
	let tree = String::from_utf8(tree).unwrap().trim().to_owned();

	let reply = call_tool(Some(repo), "show", json!({ "object": tree }));
	assert!(
		reply["result"]["isError"] != true,
		"tools/call reported an error: {reply}"
	);
	assert!(tool_text(&reply).contains("\"bad-\\377\""), "{reply}");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn mcp_merge_conflict_paths_are_reversibly_quoted() {
	use std::{ffi::OsString, os::unix::ffi::OsStringExt};

	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	run_git(repo, &["init", "-q"], None);
	run_git(repo, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
	run_git(repo, &["config", "user.name", "T"], None);
	run_git(repo, &["config", "user.email", "t@e"], None);
	let raw = b"raw-\xff".as_slice();
	let literal_collision = b"\"raw-\\377\"".as_slice();
	let write_paths = |contents: &[u8]| {
		for path in [raw, literal_collision] {
			std::fs::write(repo.join(OsString::from_vec(path.to_vec())), contents)
				.expect("write conflict path");
		}
	};

	write_paths(b"base\n");
	run_git(repo, &["add", "-A"], None);
	run_git(repo, &["commit", "-qm", "base"], None);
	run_git(repo, &["switch", "-qc", "feature"], None);
	write_paths(b"feature\n");
	run_git(repo, &["commit", "-qam", "feature"], None);
	run_git(repo, &["switch", "-q", "main"], None);
	write_paths(b"main\n");
	run_git(repo, &["commit", "-qam", "main"], None);

	let reply = call_tool(Some(repo), "merge", json!({ "commit": "feature" }));
	assert_eq!(
		reply["result"]["isError"], true,
		"expected conflict: {reply}"
	);
	let lines = tool_text(&reply).lines().collect::<Vec<_>>();
	let raw_rendered = "CONFLICT (content): Merge conflict in \"raw-\\377\"";
	let literal_rendered = "CONFLICT (content): Merge conflict in \"\\\"raw-\\\\377\\\"\"";
	assert_ne!(raw_rendered, literal_rendered);
	assert!(lines.contains(&raw_rendered), "{reply}");
	assert!(lines.contains(&literal_rendered), "{reply}");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn mcp_merge_staged_refusal_paths_are_reversibly_quoted() {
	use std::{ffi::OsString, os::unix::ffi::OsStringExt};

	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	run_git(repo, &["init", "-q"], None);
	run_git(repo, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
	run_git(repo, &["config", "user.name", "T"], None);
	run_git(repo, &["config", "user.email", "t@e"], None);
	let raw = b"raw-\xff".as_slice();
	let literal_collision = b"\"raw-\\377\"".as_slice();
	let write_paths = |contents: &[u8]| {
		for path in [raw, literal_collision] {
			std::fs::write(repo.join(OsString::from_vec(path.to_vec())), contents)
				.expect("write staged path");
		}
	};

	write_paths(b"base\n");
	run_git(repo, &["add", "-A"], None);
	run_git(repo, &["commit", "-qm", "base"], None);
	run_git(repo, &["switch", "-qc", "feature"], None);
	std::fs::write(repo.join("feature.txt"), b"feature\n").expect("write feature");
	run_git(repo, &["add", "-A"], None);
	run_git(repo, &["commit", "-qm", "feature"], None);
	run_git(repo, &["switch", "-q", "main"], None);
	std::fs::write(repo.join("main.txt"), b"main\n").expect("write main");
	run_git(repo, &["add", "-A"], None);
	run_git(repo, &["commit", "-qm", "main"], None);
	write_paths(b"staged\n");
	run_git(repo, &["add", "-A"], None);

	let reply = call_tool(Some(repo), "merge", json!({ "commit": "feature" }));
	assert_eq!(
		reply["result"]["isError"], true,
		"expected refusal: {reply}"
	);
	let lines = tool_text(&reply).lines().collect::<Vec<_>>();
	let raw_rendered = "  \"raw-\\377\"";
	let literal_rendered = "  \"\\\"raw-\\\\377\\\"\"";
	assert_ne!(raw_rendered, literal_rendered);
	assert!(lines.contains(&raw_rendered), "{reply}");
	assert!(lines.contains(&literal_rendered), "{reply}");
}

#[test]
fn mcp_revision_token_preserves_raw_tree_suffix() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	init_repo_with_raw_index_path(repo, b"bad-\xff");
	let tree = run_git(repo, &["write-tree"], None);
	let tree = String::from_utf8(tree).unwrap().trim().to_owned();
	let expected = run_git(repo, &["ls-files", "-s"], None);
	let expected = String::from_utf8(expected)
		.unwrap()
		.split_whitespace()
		.nth(1)
		.unwrap()
		.to_owned();
	let mut spec = tree.into_bytes();
	spec.extend_from_slice(b":bad-\xff");
	let token = format!(
		"gitana-revision-v1:base64url:{}",
		URL_SAFE_NO_PAD.encode(spec)
	);

	let reply = call_tool(Some(repo), "rev-parse", json!({ "spec": token }));
	assert!(
		reply["result"]["isError"] != true,
		"tools/call reported an error: {reply}"
	);
	assert_eq!(tool_text(&reply), expected);
}

#[test]
fn mcp_checkout_restore_and_reset_accept_raw_revision_suffixes() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	init_repo_with_index_paths(repo, &[b"raw-\xff/file"]);
	let tree = run_git(repo, &["write-tree"], None);
	let mut spec = tree.trim_ascii().to_vec();
	spec.extend_from_slice(b":raw-\xff");
	let token = format!(
		"gitana-revision-v1:base64url:{}",
		URL_SAFE_NO_PAD.encode(spec)
	);

	let checkout = call_tool(
		Some(repo),
		"checkout",
		json!({ "target": token, "paths": ["file"] }),
	);
	assert!(
		checkout["result"]["isError"] != true,
		"tools/call reported an error: {checkout}"
	);
	assert_eq!(std::fs::read(repo.join("file")).unwrap(), b"content\n");

	std::fs::write(repo.join("file"), b"changed\n").expect("modify restored file");
	let restore = call_tool(
		Some(repo),
		"restore",
		json!({ "source": token, "paths": ["file"] }),
	);
	assert!(
		restore["result"]["isError"] != true,
		"tools/call reported an error: {restore}"
	);
	assert_eq!(std::fs::read(repo.join("file")).unwrap(), b"content\n");

	std::fs::write(repo.join("file"), b"staged\n").expect("modify file before path reset");
	run_git(repo, &["add", "file"], None);
	let reset = call_tool(
		Some(repo),
		"reset",
		json!({ "target": token, "paths": ["file"] }),
	);
	assert!(
		reset["result"]["isError"] != true,
		"tools/call reported an error: {reset}"
	);
	assert_eq!(
		run_git(repo, &["cat-file", "-p", ":file"], None),
		b"content\n"
	);
	assert_eq!(std::fs::read(repo.join("file")).unwrap(), b"staged\n");
}

#[test]
fn mcp_missing_revision_paths_remain_reversible() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	init_repo_with_raw_index_path(repo, b"existing");
	let tree = run_git(repo, &["write-tree"], None);
	let tree = String::from_utf8(tree).unwrap().trim().as_bytes().to_vec();
	let revision_token = |suffix: &[u8]| {
		let mut spec = tree.clone();
		spec.push(b':');
		spec.extend_from_slice(suffix);
		format!(
			"gitana-revision-v1:base64url:{}",
			URL_SAFE_NO_PAD.encode(spec)
		)
	};

	let raw = call_tool(
		Some(repo),
		"rev-parse",
		json!({ "spec": revision_token(b"raw-\xff") }),
	);
	let literal = call_tool(
		Some(repo),
		"rev-parse",
		json!({ "spec": revision_token(b"\"raw-\\377\"") }),
	);
	assert_eq!(raw["result"]["isError"], true, "{raw}");
	assert_eq!(literal["result"]["isError"], true, "{literal}");
	let raw = tool_text(&raw);
	let literal = tool_text(&literal);
	assert!(
		raw.contains("path '\"raw-\\377\"' does not exist in"),
		"{raw}"
	);
	assert!(
		literal.contains("path '\"\\\"raw-\\\\377\\\"\"' does not exist in"),
		"{literal}"
	);
	assert_ne!(raw, literal);
}

#[test]
fn mcp_malformed_index_specs_remain_reversible() {
	let dir = tempfile::tempdir().expect("temp dir");
	let repo = dir.path();
	run_git(repo, &["init", "-q"], None);
	let revision_token = |spec: &[u8]| {
		format!(
			"gitana-revision-v1:base64url:{}",
			URL_SAFE_NO_PAD.encode(spec)
		)
	};

	let raw = call_tool(
		Some(repo),
		"cat-file",
		json!({ "object": revision_token(b":raw-\xff/../x") }),
	);
	let literal = call_tool(
		Some(repo),
		"cat-file",
		json!({ "object": revision_token(b":\"raw-\\377/../x\"") }),
	);
	assert_eq!(raw["result"]["isError"], true, "{raw}");
	assert_eq!(literal["result"]["isError"], true, "{literal}");
	let raw = tool_text(&raw);
	let literal = tool_text(&literal);
	assert!(
		raw.contains("invalid index revision spec: '\":raw-\\377/../x\"'"),
		"{raw}"
	);
	assert!(
		literal.contains("invalid index revision spec: '\":\\\"raw-\\\\377/../x\\\"\"'"),
		"{literal}"
	);
	assert_ne!(raw, literal);
}

fn call_tool(directory: Option<&Path>, name: &str, arguments: Value) -> Value {
	let mut command = Command::new(env!("CARGO_BIN_EXE_gta-mcp"));
	if let Some(directory) = directory {
		command.current_dir(directory);
	}
	let mut child = command
		.arg("--mcp")
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.expect("spawn gta-mcp --mcp");
	let mut stdin = child.stdin.take().expect("stdin");

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

	let send = |stdin: &mut std::process::ChildStdin, message: Value| {
		writeln!(stdin, "{message}").expect("write request");
		stdin.flush().expect("flush");
	};
	let recv = |id: u64| loop {
		let line = rx
			.recv_timeout(Duration::from_secs(20))
			.expect("a reply before timeout");
		let message: Value = serde_json::from_str(&line).expect("valid JSON-RPC message");
		if message["id"] == id {
			break message;
		}
	};

	send(
		&mut stdin,
		json!({
			"jsonrpc": "2.0",
			"id": 1,
			"method": "initialize",
			"params": {
				"protocolVersion": "2024-11-05",
				"capabilities": {},
				"clientInfo": { "name": "smoke", "version": "0" }
			}
		}),
	);
	let init = recv(1);
	assert_eq!(init["id"], 1, "initialize reply: {init}");
	send(
		&mut stdin,
		json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
	);
	send(
		&mut stdin,
		json!({
			"jsonrpc": "2.0",
			"id": 2,
			"method": "tools/call",
			"params": { "name": name, "arguments": arguments }
		}),
	);
	let reply = recv(2);

	drop(stdin);
	let _ = child.wait();
	let _ = reader.join();
	reply
}

fn tool_text(reply: &Value) -> &str {
	reply["result"]["content"]
		.as_array()
		.expect("tool content array")
		.iter()
		.find(|content| content["type"] == "text")
		.and_then(|content| content["text"].as_str())
		.expect("text tool content")
}

fn init_repo_with_raw_index_path(repo: &Path, path: &[u8]) {
	init_repo_with_index_paths(repo, &[path]);
}

fn init_repo_with_index_paths(repo: &Path, paths: &[&[u8]]) {
	run_git(repo, &["init", "-q"], None);
	let oid = run_git(repo, &["hash-object", "-w", "--stdin"], Some(b"content\n"));
	let header = format!("100644 {}\t", String::from_utf8_lossy(&oid).trim());
	let mut index_info = Vec::new();
	for path in paths {
		index_info.extend_from_slice(header.as_bytes());
		index_info.extend_from_slice(path);
		index_info.push(0);
	}
	run_git(
		repo,
		&["update-index", "-z", "--index-info"],
		Some(&index_info),
	);
}

fn path_token(path: &[u8]) -> String {
	format!("gitana-path-v1:base64url:{}", URL_SAFE_NO_PAD.encode(path))
}

fn sparse_set_error(repo: &Path, pattern: &[u8]) -> String {
	let output = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("-C")
		.arg(repo)
		.args(["sparse-checkout", "set", "--pattern"])
		.arg(path_token(pattern))
		.output()
		.expect("run gta-mcp sparse-checkout set");
	assert!(!output.status.success(), "expected sparse-checkout refusal");
	String::from_utf8(output.stderr).expect("MCP-facing sparse error must be UTF-8")
}

fn run_git(repo: &Path, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
	let mut child = Command::new("git")
		.arg("-C")
		.arg(repo)
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.expect("spawn git");
	if let Some(input) = stdin {
		child
			.stdin
			.take()
			.expect("git stdin")
			.write_all(input)
			.expect("write git stdin");
	}
	let output = child.wait_with_output().expect("wait for git");
	assert!(
		output.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	output.stdout
}
