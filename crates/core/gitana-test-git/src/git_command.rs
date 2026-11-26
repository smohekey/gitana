use std::ffi::OsString;
use std::process::Command;

/// A `git` command that behaves the same on every machine. A test adds its own arguments, and any
/// settings it relies on, explicitly.
///
/// Nothing from the machine may reach it:
/// - No global or system config file (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`). A
///   developer's `core.hooksPath`, `init.defaultRefFormat`, `index.skipHash` or
///   `status.showUntrackedFiles` otherwise changes what a test builds or what git reports.
/// - No system attributes file (`GIT_ATTR_NOSYSTEM=1`): git reads `$(prefix)/etc/gitattributes` even with
///   no system config.
/// - No default global ignore or attributes file (`XDG_CONFIG_HOME=/dev/null`): git reads
///   `~/.config/git/ignore` and `attributes` even with no global config.
/// - No inherited `GIT_*` variable but `GIT_TRACE*`. git exports `GIT_DIR` and friends to its hooks, and
///   an inherited `GIT_DIR` beats `-C`, so a suite run from a hook would work in the outer repository;
///   `GIT_CONFIG_PARAMETERS` and `GIT_CONFIG_COUNT` would add config. The trace variables stay, so what a
///   test's git does can still be traced.
pub fn git_command() -> Command {
	isolated(std::env::vars_os())
}

/// [`git_command`], given the environment the test process would pass on.
fn isolated(inherited: impl IntoIterator<Item = (OsString, OsString)>) -> Command {
	let mut command = Command::new("git");
	for (name, _) in inherited {
		let spelled = name.to_string_lossy();
		if spelled.starts_with("GIT_") && !spelled.starts_with("GIT_TRACE") {
			command.env_remove(&name);
		}
	}
	command
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env("GIT_ATTR_NOSYSTEM", "1")
		.env("XDG_CONFIG_HOME", "/dev/null");
	command
}

#[cfg(test)]
mod tests {
	use std::ffi::OsString;

	use super::{git_command, isolated};

	/// What `command` sets (`Some`) or removes (`None`) in the environment it passes on.
	fn env_of(command: &std::process::Command) -> Vec<(String, Option<String>)> {
		let mut env: Vec<_> = command
			.get_envs()
			.map(|(name, value)| {
				(
					name.to_string_lossy().into_owned(),
					value.map(|value| value.to_string_lossy().into_owned()),
				)
			})
			.collect();
		env.sort();
		env
	}

	/// The settings that keep config, attributes and ignore files from the machine out, whatever the machine
	/// has (the pin test below only notices a missing one where the machine has such a file).
	#[test]
	fn sets_the_isolating_variables() {
		let set: Vec<_> = env_of(&isolated([]));
		let expected: Vec<(String, Option<String>)> = [
			("GIT_ATTR_NOSYSTEM", "1"),
			("GIT_CONFIG_GLOBAL", "/dev/null"),
			("GIT_CONFIG_NOSYSTEM", "1"),
			("XDG_CONFIG_HOME", "/dev/null"),
		]
		.into_iter()
		.map(|(name, value)| (name.to_owned(), Some(value.to_owned())))
		.collect();
		assert_eq!(set, expected);
	}

	/// Every inherited `GIT_*` variable is removed except `GIT_TRACE*`; nothing else is touched.
	#[test]
	fn drops_inherited_git_variables_but_trace() {
		let inherited = [
			"GIT_DIR",
			"GIT_CONFIG_PARAMETERS",
			"GIT_TRACE",
			"GIT_TRACE_PACKET",
			"PATH",
			"HOME",
		]
		.map(|name| (OsString::from(name), OsString::from("x")));
		let removed: Vec<String> = env_of(&isolated(inherited))
			.into_iter()
			.filter(|(_, value)| value.is_none())
			.map(|(name, _)| name)
			.collect();
		assert_eq!(removed, ["GIT_CONFIG_PARAMETERS", "GIT_DIR"]);
	}

	/// The git a test runs reads no config from the machine: `git config --list --show-scope` in an empty
	/// directory may list only `command`-scope entries, or `local` / `worktree` ones belonging to whatever
	/// repository git finds above the temp dir (when that sits inside a checkout), which isn't what this
	/// guards. Only scopes, origins and key names are listed, so a failure never prints a developer's config
	/// values. On a machine with no global or system config this passes either way; it guards the isolation
	/// where there is some. (The default global ignore and attributes files aren't config, so this doesn't
	/// see them.)
	#[test]
	fn reads_no_machine_config() {
		let empty = std::env::temp_dir().join(format!("gitana-test-git-config-{}", std::process::id()));
		std::fs::create_dir_all(&empty).unwrap();
		let out = git_command()
			.arg("-C")
			.arg(&empty)
			.args([
				"config",
				"--list",
				"--show-scope",
				"--show-origin",
				"--name-only",
			])
			.output()
			.expect("run git");
		let _ = std::fs::remove_dir_all(&empty);
		assert!(
			out.status.success(),
			"git config --list failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		let listing = String::from_utf8_lossy(&out.stdout);
		let from_machine: Vec<&str> = listing
			.lines()
			.filter(|line| {
				!matches!(
					line.split('\t').next(),
					Some("command" | "local" | "worktree")
				)
			})
			.collect();
		assert!(
			from_machine.is_empty(),
			"git read config from the machine: {from_machine:?}"
		);
	}
}
