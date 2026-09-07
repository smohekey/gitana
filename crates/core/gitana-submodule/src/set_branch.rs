use gitana_config::{ConfigError, GitConfig};

use crate::{SetBranchOutcome, SubmoduleError, mappings_by_path};

/// Set or remove the branch associated with an exact `.gitmodules` path.
pub fn set_branch(
	config: &mut GitConfig,
	path: &str,
	branch: Option<&str>,
) -> Result<SetBranchOutcome, SubmoduleError> {
	let mappings = mappings_by_path(config)?;
	let name = mappings
		.get(path)
		.ok_or_else(|| SubmoduleError::MissingMapping(path.to_owned()))?;

	if let Some(branch) = branch {
		config.set("submodule", Some(name), "branch", branch)?;
		return Ok(SetBranchOutcome::Applied);
	}

	let values = config.get_all_raw("submodule", Some(name), "branch");
	match values.len() {
		0 => Ok(SetBranchOutcome::AlreadyDefault),
		1 => {
			config.unset("submodule", Some(name), "branch");
			Ok(SetBranchOutcome::Applied)
		}
		_ => Err(ConfigError::MultipleValues(format!("submodule.{name}.branch")).into()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config(text: &str) -> GitConfig {
		GitConfig::parse(text).unwrap()
	}

	#[test]
	fn sets_and_clears_branch_by_exact_path() {
		let mut config = config("[submodule \"one\"]\n\tpath = modules/one\n");
		assert_eq!(
			set_branch(&mut config, "modules/one", Some("bad..name")).unwrap(),
			SetBranchOutcome::Applied
		);
		assert_eq!(
			config.get_raw("submodule", Some("one"), "branch"),
			Some(Some("bad..name"))
		);
		assert!(matches!(
			set_branch(&mut config, "./modules/one", Some("main")),
			Err(SubmoduleError::MissingMapping(_))
		));
		assert_eq!(
			set_branch(&mut config, "modules/one", None).unwrap(),
			SetBranchOutcome::Applied
		);
		assert_eq!(
			set_branch(&mut config, "modules/one", None).unwrap(),
			SetBranchOutcome::AlreadyDefault
		);
	}

	#[test]
	fn preserves_multiple_branch_values() {
		for branch in [None, Some("other")] {
			let mut config =
				config("[submodule \"one\"]\n\tpath = modules/one\n\tbranch = main\n\tbranch = next\n");
			let before = config.render();
			assert!(matches!(
				set_branch(&mut config, "modules/one", branch),
				Err(SubmoduleError::Config(ConfigError::MultipleValues(_)))
			));
			assert_eq!(config.render(), before);
		}
	}

	#[test]
	fn rejects_cross_platform_mapping_aliases() {
		let mut config = config(
			"[submodule \"one\"]\n\tpath = modules/one\n\
			 [submodule \"two\"]\n\tpath = MODULES/ONE\n",
		);
		assert!(matches!(
			set_branch(&mut config, "modules/one", Some("main")),
			Err(SubmoduleError::AmbiguousDeclaration(_))
		));
	}
}
