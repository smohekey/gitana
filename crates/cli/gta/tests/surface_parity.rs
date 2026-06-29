//! Guards TODO item "keep `gta` and `gta-mcp` command surfaces in lockstep". The two CLIs are
//! separate clap definitions that must be edited together; this test fails if they drift.
//!
//! It compares a *normalized* spec of each clap command tree — recursively, including the root's
//! own (global) arguments and any nested subcommands. For every argument it checks the id plus the
//! semantic properties: required-ness, action (which captures arity: flag / single / repeatable /
//! counted), global-ness, allowed values, and defaults. Argument *groups* (mutual exclusion /
//! required-one) are compared too.
//!
//! Deliberately excluded are the two surfaces' **intended** differences: presentation (`gta` uses
//! positionals where `gta-mcp` uses `--named` arguments — so long/short flags, positional-ness,
//! value names, and the raw `num_args` that encodes them are not compared; `action` carries the
//! semantic arity instead), and the clap-mcp serving flags that exist on `gta-mcp` only. Raw value
//! parsers (e.g. `String` vs `PathBuf`) are not compared either — only restricted `possible_values`,
//! which is the surface-affecting part.

use std::collections::{BTreeMap, BTreeSet};

/// clap-mcp injects these top-level arguments on `gta-mcp` only (to select stdio/HTTP serving).
/// They are not part of the shared command surface.
const MCP_ONLY_ARG_IDS: &[&str] = &["mcp", "mcp-http"];

#[test]
fn gta_and_mcp_expose_the_same_surface() {
	let gta = CommandSpec::of(&gta::clap_command());
	let mcp = CommandSpec::of(&gta_mcp::clap_command());

	let mut diffs = Vec::new();
	gta.diff("gta", &mcp, &mut diffs);
	assert!(
		diffs.is_empty(),
		"`gta` and `gta-mcp` command surfaces have drifted:\n  {}",
		diffs.join("\n  "),
	);
}

/// The presentation-agnostic shape of one (sub)command: its arguments, groups, and subcommands.
struct CommandSpec {
	args: BTreeMap<String, ArgSpec>,
	groups: BTreeMap<String, GroupSpec>,
	subcommands: BTreeMap<String, CommandSpec>,
}

/// The semantic properties of one argument, ignoring how it is presented on the command line.
#[derive(PartialEq, Eq, Debug)]
struct ArgSpec {
	required: bool,
	/// Debug of the clap `ArgAction` — `SetTrue` (flag), `Set` (one value), `Append`
	/// (repeatable), `Count`. Captures arity without the positional-vs-named `num_args` difference.
	action: String,
	global: bool,
	possible_values: Vec<String>,
	default_values: Vec<String>,
}

#[derive(PartialEq, Eq, Debug)]
struct GroupSpec {
	args: BTreeSet<String>,
	required: bool,
	multiple: bool,
}

impl CommandSpec {
	fn of(command: &clap::Command) -> Self {
		let args = command
			.get_arguments()
			.filter(|arg| {
				let id = arg.get_id().as_str();
				// Drop clap's auto-injected help/version and the mcp-only serving flags.
				id != "help" && id != "version" && !MCP_ONLY_ARG_IDS.contains(&id)
			})
			.map(|arg| (arg.get_id().as_str().to_owned(), ArgSpec::of(arg)))
			.collect();
		let groups = command
			.get_groups()
			.map(|group| (group.get_id().as_str().to_owned(), GroupSpec::of(group)))
			.collect();
		let subcommands = command
			.get_subcommands()
			.filter(|sub| sub.get_name() != "help")
			.map(|sub| (sub.get_name().to_owned(), CommandSpec::of(sub)))
			.collect();
		CommandSpec {
			args,
			groups,
			subcommands,
		}
	}

	/// Collect human-readable differences between this (`gta`) spec and the `gta-mcp` spec, prefixing
	/// each with the command path it was found at.
	fn diff(&self, path: &str, mcp: &Self, out: &mut Vec<String>) {
		diff_keys(path, "argument", &self.args, &mcp.args, out);
		for (id, gta_arg) in &self.args {
			if let Some(mcp_arg) = mcp.args.get(id)
				&& gta_arg != mcp_arg
			{
				out.push(format!(
					"{path}: argument `{id}` differs: gta={gta_arg:?} vs gta-mcp={mcp_arg:?}"
				));
			}
		}

		diff_keys(path, "group", &self.groups, &mcp.groups, out);
		for (id, gta_group) in &self.groups {
			if let Some(mcp_group) = mcp.groups.get(id)
				&& gta_group != mcp_group
			{
				out.push(format!(
					"{path}: group `{id}` differs: gta={gta_group:?} vs gta-mcp={mcp_group:?}"
				));
			}
		}

		diff_keys(path, "subcommand", &self.subcommands, &mcp.subcommands, out);
		for (name, gta_sub) in &self.subcommands {
			if let Some(mcp_sub) = mcp.subcommands.get(name) {
				gta_sub.diff(&format!("{path} {name}"), mcp_sub, out);
			}
		}
	}
}

impl ArgSpec {
	fn of(arg: &clap::Arg) -> Self {
		let mut possible_values: Vec<String> = arg
			.get_possible_values()
			.iter()
			.map(|value| value.get_name().to_owned())
			.collect();
		possible_values.sort();
		let default_values = arg
			.get_default_values()
			.iter()
			.map(|value| value.to_string_lossy().into_owned())
			.collect();
		ArgSpec {
			required: arg.is_required_set(),
			action: format!("{:?}", arg.get_action()),
			global: arg.is_global_set(),
			possible_values,
			default_values,
		}
	}
}

impl GroupSpec {
	fn of(group: &clap::ArgGroup) -> Self {
		// `ArgGroup::is_multiple` takes `&mut self`, so probe it on a clone.
		let mut multiple_probe = group.clone();
		GroupSpec {
			args: group.get_args().map(|id| id.as_str().to_owned()).collect(),
			required: group.is_required_set(),
			multiple: multiple_probe.is_multiple(),
		}
	}
}

/// Report keys present on only one side (`gta` vs `gta-mcp`).
fn diff_keys<V>(
	path: &str,
	kind: &str,
	gta: &BTreeMap<String, V>,
	mcp: &BTreeMap<String, V>,
	out: &mut Vec<String>,
) {
	for key in gta.keys().filter(|key| !mcp.contains_key(*key)) {
		out.push(format!("{path}: {kind} `{key}` only in gta"));
	}
	for key in mcp.keys().filter(|key| !gta.contains_key(*key)) {
		out.push(format!("{path}: {kind} `{key}` only in gta-mcp"));
	}
}
