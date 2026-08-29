use gitana_config::GitConfig;

use crate::SubmoduleError;

/// One `[submodule "name"]` declaration from `.gitmodules`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleDeclaration {
	pub name: String,
	pub path: String,
	pub url: Option<String>,
	pub branch: Option<String>,
	pub update: Option<String>,
}

impl SubmoduleDeclaration {
	pub fn parse_all(text: &str) -> Result<Vec<Self>, SubmoduleError> {
		let config = GitConfig::parse(text)?;
		let mut declarations = Vec::new();
		for name in config.subsections("submodule") {
			let path = field(&config, name, "path", true)?;
			let url = field(&config, name, "url", true)?;
			let branch = field(&config, name, "branch", false)?;
			let update = field(&config, name, "update", false)?;
			let Some(path) = path else {
				continue;
			};
			declarations.push(Self {
				name: name.to_owned(),
				path,
				url,
				branch,
				update,
			});
		}
		Ok(declarations)
	}
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
}
