use std::collections::{HashMap, HashSet};

use caseless::Caseless;
use gitana_config::GitConfig;
use unicode_normalization::UnicodeNormalization;

use crate::SubmoduleError;

/// One `[submodule "name"]` declaration from `.gitmodules`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleDeclaration {
	pub name: String,
	pub path: String,
	pub url: Option<String>,
	pub branch: Option<String>,
	pub update: Option<String>,
	/// Recommended shallow-clone policy from `.gitmodules`.
	pub shallow: Option<bool>,
}

impl SubmoduleDeclaration {
	pub fn parse_all(text: &str) -> Result<Vec<Self>, SubmoduleError> {
		let config = GitConfig::parse(text)?;
		Self::from_config(&config)
	}

	pub(crate) fn from_config(config: &GitConfig) -> Result<Vec<Self>, SubmoduleError> {
		let mut declarations = Vec::new();
		for name in config.subsections("submodule") {
			let path = field(config, name, "path", true)?;
			let url = field(config, name, "url", true)?;
			let branch = field(config, name, "branch", false)?;
			let update = field(config, name, "update", false)?;
			let shallow = config.get_bool_validated("submodule", Some(name), "shallow")?;
			let Some(path) = path else {
				continue;
			};
			declarations.push(Self {
				name: name.to_owned(),
				path,
				url,
				branch,
				update,
				shallow,
			});
		}
		Ok(declarations)
	}
}

pub(crate) fn mappings_by_path(
	config: &GitConfig,
) -> Result<HashMap<String, String>, SubmoduleError> {
	let mut by_path = HashMap::new();
	let mut names: HashSet<String> = HashSet::new();
	let mut paths: HashSet<String> = HashSet::new();
	for name in config.subsections("submodule") {
		let Some(path) = field(config, name, "path", true)? else {
			continue;
		};
		validate_name(name)?;
		validate_path(&path)?;
		if by_path.contains_key(&path) {
			return Err(SubmoduleError::DuplicateMapping(path));
		}
		let name_key = filesystem_key(name);
		let path_key = filesystem_key(&path);
		if names
			.iter()
			.any(|known| component_prefix_collision(known, &name_key))
			|| paths
				.iter()
				.any(|known| component_prefix_collision(known, &path_key))
		{
			return Err(SubmoduleError::AmbiguousDeclaration(path));
		}
		names.insert(name_key);
		paths.insert(path_key);
		by_path.insert(path, name.to_owned());
	}
	Ok(by_path)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn declarations_by_path(
	declarations: Vec<SubmoduleDeclaration>,
) -> Result<HashMap<String, SubmoduleDeclaration>, SubmoduleError> {
	let mut by_path = HashMap::with_capacity(declarations.len());
	let mut names: HashSet<String> = HashSet::new();
	let mut paths: HashSet<String> = HashSet::new();
	for declaration in declarations {
		validate_name(&declaration.name)?;
		validate_path(&declaration.path)?;
		let path = declaration.path.clone();
		if by_path.contains_key(&path) {
			return Err(SubmoduleError::DuplicateMapping(path));
		}
		let name_key = filesystem_key(&declaration.name);
		let path_key = filesystem_key(&declaration.path);
		if names
			.iter()
			.any(|name| component_prefix_collision(name, &name_key))
			|| paths
				.iter()
				.any(|path| component_prefix_collision(path, &path_key))
		{
			return Err(SubmoduleError::AmbiguousDeclaration(declaration.path));
		}
		names.insert(name_key);
		paths.insert(path_key);
		by_path.insert(path, declaration);
	}
	Ok(by_path)
}

fn filesystem_key(value: &str) -> String {
	value.chars().nfd().default_case_fold().nfd().collect()
}

fn component_prefix_collision(left: &str, right: &str) -> bool {
	left == right
		|| right
			.strip_prefix(left)
			.is_some_and(|remainder| remainder.starts_with('/'))
		|| left
			.strip_prefix(right)
			.is_some_and(|remainder| remainder.starts_with('/'))
}

pub(crate) fn validate_name(name: &str) -> Result<(), SubmoduleError> {
	if safe_relative(name) {
		Ok(())
	} else {
		Err(SubmoduleError::UnsafeName(name.to_owned()))
	}
}

pub(crate) fn validate_path(path: &str) -> Result<(), SubmoduleError> {
	if safe_relative(path) {
		Ok(())
	} else {
		Err(SubmoduleError::UnsafePath(path.to_owned()))
	}
}

fn safe_relative(value: &str) -> bool {
	if value.is_empty() || value.starts_with('/') {
		return false;
	}
	if cfg!(windows) && (value.contains('\\') || value.contains(':')) {
		return false;
	}
	value.split('/').all(|component| {
		!component.is_empty()
			&& !matches!(component, "." | "..")
			&& !is_ntfs_dot_git_alias(component)
			&& !component.chars().any(char::is_control)
	})
}

fn is_ntfs_dot_git_alias(component: &str) -> bool {
	let bytes = component.as_bytes();
	let remainder = if bytes
		.get(..4)
		.is_some_and(|prefix| prefix.eq_ignore_ascii_case(b".git"))
	{
		&bytes[4..]
	} else if bytes
		.get(..5)
		.is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"git~1"))
	{
		&bytes[5..]
	} else {
		return false;
	};

	for byte in remainder {
		if *byte == b':' {
			return true;
		}
		if !matches!(*byte, b' ' | b'.') {
			return false;
		}
	}
	true
}

fn field(
	config: &GitConfig,
	name: &str,
	key: &str,
	guard_options: bool,
) -> Result<Option<String>, SubmoduleError> {
	let values = config.get_all_raw("submodule", Some(name), key);
	if values.iter().any(Option::is_none) {
		return Err(SubmoduleError::MissingValue(format!(
			"submodule.{name}.{key}"
		)));
	}
	Ok(
		values
			.into_iter()
			.flatten()
			.rfind(|value| !guard_options || !value.starts_with('-'))
			.map(str::to_owned),
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_in_order_with_last_valid_value() {
		let declarations = SubmoduleDeclaration::parse_all(
			"[submodule \"a\"]\n\tpath = libs/a\n\turl = good\n\turl = -bad\n",
		)
		.unwrap();
		assert_eq!(declarations[0].name, "a");
		assert_eq!(declarations[0].path, "libs/a");
		assert_eq!(declarations[0].url.as_deref(), Some("good"));
	}

	#[test]
	fn rejects_any_valueless_string_field() {
		let error =
			SubmoduleDeclaration::parse_all("[submodule \"a\"]\n\tpath = a\n\turl\n\turl = good\n")
				.unwrap_err();
		assert!(error.to_string().contains("missing value"));
	}

	#[test]
	fn parses_shallow_with_gits_boolean_grammar() {
		let declarations = SubmoduleDeclaration::parse_all(
			"[submodule \"true\"]\n\tpath = true\n\tshallow\n\
			 [submodule \"false\"]\n\tpath = false\n\tshallow = off\n",
		)
		.unwrap();
		assert_eq!(declarations[0].shallow, Some(true));
		assert_eq!(declarations[1].shallow, Some(false));
	}

	#[test]
	fn validates_every_shallow_occurrence() {
		let error = SubmoduleDeclaration::parse_all(
			"[submodule \"a\"]\n\tpath = a\n\tshallow = true\n\
			 [submodule \"unselected\"]\n\tshallow = invalid\n\tshallow = false\n",
		)
		.unwrap_err();
		assert!(error.to_string().contains("not a boolean"));
	}

	#[test]
	fn rejects_traversal_and_git_components() {
		for value in [
			"",
			"../x",
			"a/../x",
			".git",
			"a/.GIT/x",
			"a//b",
			".git ",
			"a/.GiT.../x",
			"git~1",
			"a/GIT~1./x",
			"a/.git  :stream/x",
		] {
			assert!(!safe_relative(value), "{value:?}");
		}
		for value in ["git~2", ".gitx", "a/agit~1/b", "a/.git x/b"] {
			assert!(safe_relative(value), "{value:?}");
		}
	}
}
