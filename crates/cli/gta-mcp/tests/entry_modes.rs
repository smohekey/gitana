//! Compatibility checks for the non-command entry modes owned by the custom MCP bridge.

use std::path::Path;
use std::process::Command;

#[test]
fn skills_export_supports_separate_and_attached_directories() {
	let temporary = tempfile::tempdir().expect("temporary export root");
	let separate = temporary.path().join("separate");
	let output = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg("--export-skills")
		.arg(&separate)
		.output()
		.expect("run separate skills export");
	assert!(
		output.status.success(),
		"separate skills export failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_custom_surface(&separate);

	let attached = temporary.path().join("attached");
	let output = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.arg(format!("--export-skills={}", attached.display()))
		.output()
		.expect("run attached skills export");
	assert!(
		output.status.success(),
		"attached skills export failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_custom_surface(&attached);
}

#[test]
fn one_shot_scoped_keyed_read_ignores_invalid_command_config() {
	let temporary = tempfile::tempdir().expect("temporary config root");
	let global = temporary.path().join("global.config");
	std::fs::write(&global, "[user]\n\tname = FromGlobal\n").unwrap();

	let output = Command::new(env!("CARGO_BIN_EXE_gta-mcp"))
		.args(["-C", temporary.path().to_str().unwrap()])
		.args(["-c", "invalid"])
		.args(["config", "--global", "--name", "user.name"])
		.env("GIT_CONFIG_GLOBAL", &global)
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.output()
		.expect("run one-shot scoped config read");
	assert!(
		output.status.success(),
		"scoped config read failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(String::from_utf8_lossy(&output.stdout), "FromGlobal\n");
}

fn assert_custom_surface(output: &Path) {
	let application = output.join("gta-mcp");
	let worktree =
		std::fs::read_to_string(application.join("worktree-add/SKILL.md")).expect("worktree_add skill");
	assert!(worktree.contains("allowed-tools: worktree_add"));
	let submodule = std::fs::read_to_string(application.join("submodule-status/SKILL.md"))
		.expect("submodule_status skill");
	assert!(submodule.contains("allowed-tools: submodule_status"));
	let status =
		std::fs::read_to_string(application.join("status/SKILL.md")).expect("top-level status skill");
	assert!(status.contains("allowed-tools: status"));
}
