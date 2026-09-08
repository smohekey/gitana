use gitana_config::{ConfigError, GitConfig};

use crate::{SubmoduleError, mappings_by_path};

/// Set the URL associated with an exact `.gitmodules` path.
pub fn set_url(config: &mut GitConfig, path: &str, url: &str) -> Result<String, SubmoduleError> {
	let mappings = mappings_by_path(config)?;
	let name = mappings
		.get(path)
		.ok_or_else(|| SubmoduleError::MissingMapping(path.to_owned()))?
		.clone();
	if config.get_all_raw("submodule", Some(&name), "url").len() > 1 {
		return Err(ConfigError::MultipleValues(format!("submodule.{name}.url")).into());
	}
	config.set("submodule", Some(&name), "url", url)?;
	Ok(name)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config(text: &str) -> GitConfig {
		GitConfig::parse(text).unwrap()
	}

	#[test]
	fn sets_missing_or_existing_url_by_exact_path() {
		let mut config = config("[submodule \"one\"]\n\tpath = modules/one\n");
		assert_eq!(set_url(&mut config, "modules/one", "").unwrap(), "one");
		assert_eq!(
			config.get_raw("submodule", Some("one"), "url"),
			Some(Some(""))
		);
		assert!(matches!(
			set_url(&mut config, "./modules/one", "next"),
			Err(SubmoduleError::MissingMapping(_))
		));
	}

	#[test]
	fn preserves_multiple_url_values() {
		let mut config =
			config("[submodule \"one\"]\n\tpath = modules/one\n\turl = first\n\turl = second\n");
		let before = config.render();
		assert!(matches!(
			set_url(&mut config, "modules/one", "next"),
			Err(SubmoduleError::Config(ConfigError::MultipleValues(_)))
		));
		assert_eq!(config.render(), before);
	}
}
