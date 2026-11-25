use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use gitana_config::{GitConfigSource, parse_bool};
use gitana_path::GitPath;
use gitana_worktree::IgnoreSubmodules;

/// A `.gitmodules`, as git's `status` reads it: which declaration names each submodule path, and
/// the ignore setting each declares. Reading it is fatal, warns, or passes exactly where git's
/// status is, warns, or passes (probed vs git 2.55); every declaration is checked, not just those
/// of the submodules looked at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Declarations {
	/// Each declared path's declaration name. With two declarations for one path, the later.
	names: HashMap<String, String>,
	/// Each declaration's `ignore` setting, when it has a valid one.
	ignore: HashMap<String, IgnoreSubmodules>,
}

impl Declarations {
	/// The declarations in `text`, adding git's warnings to `warnings`. An error is git's fatal.
	pub(crate) fn parse(text: &str, warnings: &mut Vec<String>) -> Result<Self> {
		let config =
			GitConfigSource::parse(text).map_err(|error| anyhow!("bad .gitmodules: {error}"))?;
		let mut declarations = Self::default();
		for name in config.subsections("submodule") {
			// git skips a suspicious name's whole declaration, unread.
			if suspicious(name) {
				warnings.push(format!("ignoring suspicious submodule name: {name}"));
				continue;
			}
			let values = |key: &str| config.get_all_raw("submodule", Some(name), key);
			for key in ["path", "url", "branch", "update", "ignore"] {
				if values(key).iter().any(Option::is_none) {
					bail!("missing value for 'submodule.{name}.{key}'");
				}
			}
			for update in values("update").into_iter().flatten() {
				// `.gitmodules` can't name a command (`!…`), which git refuses there too.
				if !matches!(update, "checkout" | "rebase" | "merge" | "none") {
					bail!("invalid value for 'submodule.{name}.update'");
				}
			}
			for fetch in values("fetchrecursesubmodules").into_iter().flatten() {
				if fetch != "on-demand" && parse_bool(Some(fetch)).is_err() {
					bail!("bad submodule.{name}.fetchrecursesubmodules argument: {fetch}");
				}
			}
			for shallow in values("shallow") {
				if parse_bool(shallow).is_err() {
					bail!(
						"bad boolean config value '{}' for 'submodule.{name}.shallow'",
						shallow.unwrap_or_default()
					);
				}
			}
			for url in values("url").into_iter().flatten() {
				if url.starts_with('-') {
					warnings.push(option_like(name, "url", url));
				}
			}
			let mut path = None;
			for value in values("path").into_iter().flatten() {
				if value.starts_with('-') {
					warnings.push(option_like(name, "path", value));
				} else {
					path = Some(value);
				}
			}
			for value in values("ignore").into_iter().flatten() {
				match IgnoreSubmodules::parse(value) {
					Some(ignore) => {
						declarations.ignore.insert(name.to_owned(), ignore);
					}
					None => warnings.push(format!(
						"Invalid parameter '{value}' for config option 'submodule.{name}.ignore'"
					)),
				}
			}
			if let Some(path) = path {
				declarations.names.insert(path.to_owned(), name.to_owned());
			}
		}
		Ok(declarations)
	}

	/// The name of the declaration for the submodule at `path`, if one declares it.
	pub(crate) fn name(&self, path: &GitPath) -> Option<&str> {
		self.names.get(path.as_utf8()?).map(String::as_str)
	}

	/// The `ignore` setting the declaration `name` gives, if it gives a valid one.
	pub(crate) fn ignore(&self, name: &str) -> Option<IgnoreSubmodules> {
		self.ignore.get(name).copied()
	}
}

/// git's warning for a `.gitmodules` value that could be read as a command-line option, which it
/// then ignores.
fn option_like(name: &str, key: &str, value: &str) -> String {
	format!(
		"ignoring 'submodule.{name}.{key}' which may be interpreted as a command-line option: {value}"
	)
}

/// git's `check_submodule_name`: a name that's empty, or has `..` as a component (split on `/` and
/// `\` on every platform), could escape `modules/`.
fn suspicious(name: &str) -> bool {
	name.is_empty() || name.split(['/', '\\']).any(|component| component == "..")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(text: &str) -> (Result<Declarations>, Vec<String>) {
		let mut warnings = Vec::new();
		(Declarations::parse(text, &mut warnings), warnings)
	}

	fn path(path: &str) -> GitPath {
		GitPath::from_utf8(path).unwrap()
	}

	#[test]
	fn maps_paths_to_names_and_their_ignore_settings() {
		let (declarations, warnings) = parse(
			"[submodule \"named\"]\n\tpath = sub\n\turl = ../sub\n\tignore = dirty\n\
			 [submodule \"other\"]\n\tpath = other\n",
		);
		let declarations = declarations.unwrap();
		assert_eq!(warnings, Vec::<String>::new());
		assert_eq!(declarations.name(&path("sub")), Some("named"));
		assert_eq!(declarations.ignore("named"), Some(IgnoreSubmodules::Dirty));
		assert_eq!(declarations.name(&path("other")), Some("other"));
		assert_eq!(declarations.ignore("other"), None);
		assert_eq!(declarations.name(&path("missing")), None);
	}

	#[test]
	fn fails_where_git_s_status_fails() {
		for (text, message) in [
			("[submodule \"sub\"\n", "bad .gitmodules"),
			(
				"[submodule \"s\"]\n\tpath\n",
				"missing value for 'submodule.s.path'",
			),
			(
				"[submodule \"s\"]\n\turl\n",
				"missing value for 'submodule.s.url'",
			),
			(
				"[submodule \"s\"]\n\tbranch\n",
				"missing value for 'submodule.s.branch'",
			),
			(
				"[submodule \"s\"]\n\tupdate\n",
				"missing value for 'submodule.s.update'",
			),
			(
				"[submodule \"s\"]\n\tignore\n",
				"missing value for 'submodule.s.ignore'",
			),
			(
				"[submodule \"s\"]\n\tupdate = bogus\n",
				"invalid value for 'submodule.s.update'",
			),
			(
				"[submodule \"s\"]\n\tupdate = !cmd\n",
				"invalid value for 'submodule.s.update'",
			),
			(
				"[submodule \"s\"]\n\tfetchRecurseSubmodules = bogus\n",
				"bad submodule.s.fetchrecursesubmodules argument: bogus",
			),
			(
				"[submodule \"s\"]\n\tshallow = invalid\n",
				"bad boolean config value 'invalid' for 'submodule.s.shallow'",
			),
		] {
			let (declarations, _) = parse(text);
			let error = declarations.expect_err(text).to_string();
			assert!(error.starts_with(message), "{text:?}: {error}");
		}
	}

	#[test]
	fn warns_where_git_warns() {
		let (declarations, warnings) = parse(
			"[submodule \"../evil\"]\n\tpath = evil\n\tshallow = invalid\n\
			 [submodule \"s\"]\n\tpath = -s\n\turl = -u\n\tignore = bogus\n",
		);
		let declarations = declarations.unwrap();
		assert_eq!(
			warnings,
			[
				"ignoring suspicious submodule name: ../evil",
				"ignoring 'submodule.s.url' which may be interpreted as a command-line option: -u",
				"ignoring 'submodule.s.path' which may be interpreted as a command-line option: -s",
				"Invalid parameter 'bogus' for config option 'submodule.s.ignore'",
			]
		);
		assert_eq!(declarations.name(&path("evil")), None, "skipped, unread");
		assert_eq!(declarations.name(&path("-s")), None, "its path is ignored");
		assert_eq!(declarations.ignore("s"), None);
	}

	#[test]
	fn passes_what_git_passes() {
		let (declarations, warnings) = parse(
			"[submodule \"s\"]\n\tpath = s\n\tfetchRecurseSubmodules\n\tshallow\n\tactive = bogus\n\
			 \tbranch = -bad\n\tfetchRecurseSubmodules = on-demand\n\tupdate = rebase\n\
			 [submodule \"dup\"]\n\tpath = s\n",
		);
		let declarations = declarations.unwrap();
		assert_eq!(warnings, Vec::<String>::new());
		assert_eq!(declarations.name(&path("s")), Some("dup"), "the later one");
	}

	#[test]
	fn a_later_valid_ignore_wins_and_an_invalid_one_is_skipped() {
		let (declarations, _) =
			parse("[submodule \"s\"]\n\tpath = s\n\tignore = all\n\tignore = bogus\n");
		assert_eq!(
			declarations.unwrap().ignore("s"),
			Some(IgnoreSubmodules::All)
		);
		let (declarations, _) =
			parse("[submodule \"s\"]\n\tpath = s\n\tignore = all\n\tignore = dirty\n");
		assert_eq!(
			declarations.unwrap().ignore("s"),
			Some(IgnoreSubmodules::Dirty)
		);
	}

	#[test]
	fn names_git_finds_suspicious() {
		for name in ["", "..", "../x", "a/..", "a/../b", "a\\..\\b", "..\\x"] {
			assert!(suspicious(name), "{name:?}");
		}
		for name in ["a", "a/b", "..a", "a..", "a/..b", ".", "./a"] {
			assert!(!suspicious(name), "{name:?}");
		}
	}
}
