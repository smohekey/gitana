use crate::{ConfigError, parser};

/// One resolved variable. `section` and `name` are stored lower-cased (git folds
/// their case); `subsection` is kept verbatim (case-sensitive). A `None` value is a
/// boolean-true variable (`name` with no `= value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Variable {
	pub section: String,
	pub subsection: Option<String>,
	pub name: String,
	pub value: Option<String>,
}

/// A parsed git configuration: an ordered list of variables with case-correct,
/// multi-value-aware lookups. Includes/`includeIf` are not handled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitConfig {
	variables: Vec<Variable>,
}

impl GitConfig {
	/// An empty config.
	pub fn new() -> Self {
		Self::default()
	}

	/// Parse git config text.
	pub fn parse(text: &str) -> Result<Self, ConfigError> {
		parser::parse(text)
	}

	pub(crate) fn from_variables(variables: Vec<Variable>) -> Self {
		Self { variables }
	}

	fn matches<'a>(
		&'a self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> impl Iterator<Item = &'a Variable> {
		let section = section.to_ascii_lowercase();
		let name = name.to_ascii_lowercase();
		let subsection = subsection.map(str::to_owned);
		self.variables.iter().filter(move |v| {
			v.section == section && v.name == name && v.subsection.as_deref() == subsection.as_deref()
		})
	}

	/// The last value for a variable, or `None` if unset (or set as boolean-true).
	pub fn get_string(&self, section: &str, subsection: Option<&str>, name: &str) -> Option<&str> {
		self
			.matches(section, subsection, name)
			.last()
			.and_then(|v| v.value.as_deref())
	}

	/// All values for a multi-valued variable, in order.
	pub fn get_all(&self, section: &str, subsection: Option<&str>, name: &str) -> Vec<&str> {
		self
			.matches(section, subsection, name)
			.filter_map(|v| v.value.as_deref())
			.collect()
	}

	/// Interpret the last value as a git boolean (`true/yes/on/1`, `false/no/off/0/""`,
	/// or a bare name as true). `None` if the variable is unset.
	pub fn get_bool(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Result<Option<bool>, ConfigError> {
		match self.matches(section, subsection, name).last() {
			None => Ok(None),
			Some(v) => interpret_bool(v.value.as_deref()).map(Some),
		}
	}

	/// Interpret the last value as a git integer (optional `k`/`m`/`g` 1024-multiplier).
	pub fn get_int(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Result<Option<i64>, ConfigError> {
		match self.get_string(section, subsection, name) {
			None => Ok(None),
			Some(v) => interpret_int(v).map(Some),
		}
	}

	/// Replace all values of a variable with a single value.
	pub fn set(&mut self, section: &str, subsection: Option<&str>, name: &str, value: &str) {
		let section_lc = section.to_ascii_lowercase();
		let name_lc = name.to_ascii_lowercase();
		self.variables.retain(|v| {
			!(v.section == section_lc && v.name == name_lc && v.subsection.as_deref() == subsection)
		});
		self.variables.push(Variable {
			section: section_lc,
			subsection: subsection.map(str::to_owned),
			name: name_lc,
			value: Some(value.to_owned()),
		});
	}

	/// Append a value (for multi-valued variables); `None` value is boolean-true.
	pub fn add(&mut self, section: &str, subsection: Option<&str>, name: &str, value: Option<&str>) {
		self.variables.push(Variable {
			section: section.to_ascii_lowercase(),
			subsection: subsection.map(str::to_owned),
			name: name.to_ascii_lowercase(),
			value: value.map(str::to_owned),
		});
	}

	/// Serialise to git config text (tab-indented variables under their headers).
	pub fn render(&self) -> String {
		let mut out = String::new();
		let mut current: Option<(&str, Option<&str>)> = None;
		for v in &self.variables {
			let here = (v.section.as_str(), v.subsection.as_deref());
			if current != Some(here) {
				match v.subsection.as_deref() {
					Some(sub) => out.push_str(&format!("[{} \"{}\"]\n", v.section, escape_subsection(sub))),
					None => out.push_str(&format!("[{}]\n", v.section)),
				}
				current = Some(here);
			}
			match &v.value {
				Some(value) => out.push_str(&format!("\t{} = {}\n", v.name, quote_value(value))),
				None => out.push_str(&format!("\t{}\n", v.name)),
			}
		}
		out
	}
}

fn interpret_bool(value: Option<&str>) -> Result<bool, ConfigError> {
	match value {
		None => Ok(true),
		Some(v) => match v.to_ascii_lowercase().as_str() {
			"true" | "yes" | "on" | "1" => Ok(true),
			"false" | "no" | "off" | "0" | "" => Ok(false),
			_ => Err(ConfigError::NotBool(v.to_owned())),
		},
	}
}

fn interpret_int(value: &str) -> Result<i64, ConfigError> {
	let trimmed = value.trim();
	let (digits, scale) = match trimmed.chars().last() {
		Some('k' | 'K') => (&trimmed[..trimmed.len() - 1], 1024),
		Some('m' | 'M') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
		Some('g' | 'G') => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
		_ => (trimmed, 1),
	};
	digits
		.trim()
		.parse::<i64>()
		.ok()
		.and_then(|n| n.checked_mul(scale))
		.ok_or_else(|| ConfigError::NotInt(value.to_owned()))
}

fn quote_value(value: &str) -> String {
	let needs_quote = value.is_empty()
		|| value.starts_with([' ', '\t'])
		|| value.ends_with([' ', '\t'])
		|| value.contains(['"', '\\', '#', ';', '\n', '\t']);
	if !needs_quote {
		return value.to_owned();
	}
	let mut out = String::from('"');
	for c in value.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\t' => out.push_str("\\t"),
			other => out.push(other),
		}
	}
	out.push('"');
	out
}

fn escape_subsection(sub: &str) -> String {
	let mut out = String::new();
	for c in sub.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			other => out.push(other),
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn int_suffixes() {
		assert_eq!(interpret_int("1").unwrap(), 1);
		assert_eq!(interpret_int("1k").unwrap(), 1024);
		assert_eq!(interpret_int(" 2M ").unwrap(), 2 * 1024 * 1024);
		assert!(interpret_int("nope").is_err());
	}

	#[test]
	fn bool_forms() {
		assert!(interpret_bool(None).unwrap());
		assert!(interpret_bool(Some("Yes")).unwrap());
		assert!(!interpret_bool(Some("off")).unwrap());
		assert!(!interpret_bool(Some("")).unwrap());
		assert!(interpret_bool(Some("maybe")).is_err());
	}

	#[test]
	fn set_get_render_round_trip() {
		let mut config = GitConfig::new();
		config.set("core", None, "repositoryformatversion", "1");
		config.set("core", None, "bare", "false");
		config.set("extensions", None, "objectFormat", "sha256");

		assert_eq!(
			config
				.get_int("core", None, "repositoryformatversion")
				.unwrap(),
			Some(1)
		);
		// Key lookup is case-insensitive.
		assert_eq!(
			config.get_string("Extensions", None, "objectformat"),
			Some("sha256")
		);

		let reparsed = GitConfig::parse(&config.render()).unwrap();
		assert_eq!(reparsed, config);
	}
}
