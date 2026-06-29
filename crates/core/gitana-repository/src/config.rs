use gitana_config::GitConfig;

use crate::RepositoryError;

/// The repository identity the engine cares about, read from / written to the git
/// `config` file via [`GitConfig`]. gitana is sha256-only; [`Config::parse`] refuses
/// anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
	/// `core.repositoryformatversion` (1 for a sha256 repo).
	pub repository_format_version: u32,
	/// `extensions.objectformat` (`sha256`).
	pub object_format: String,
}

impl Config {
	/// The config gitana writes on `init`: a git-recognised sha256 repository.
	pub fn sha256() -> Self {
		Config {
			repository_format_version: 1,
			object_format: "sha256".to_owned(),
		}
	}

	/// Render to git config text.
	pub fn render(&self) -> String {
		// Each key is set exactly once on a fresh config, so `set` cannot hit the multi-value guard.
		let mut config = GitConfig::new();
		let set = |config: &mut GitConfig, section, subsection, name, value: &str| {
			config
				.set(section, subsection, name, value)
				.expect("unique key on a fresh config");
		};
		let version = self.repository_format_version.to_string();
		set(
			&mut config,
			"core",
			None,
			"repositoryformatversion",
			&version,
		);
		set(&mut config, "core", None, "filemode", "true");
		set(&mut config, "core", None, "bare", "false");
		set(
			&mut config,
			"extensions",
			None,
			"objectformat",
			&self.object_format,
		);
		config.render()
	}

	/// Parse a git config and validate the repository is a supported sha256 repo.
	pub fn parse(text: &str) -> Result<Self, RepositoryError> {
		let config = GitConfig::parse(text)
			.map_err(|error| RepositoryError::UnsupportedFormat(error.to_string()))?;

		let object_format = config
			.get_string("extensions", None, "objectformat")
			.unwrap_or("sha1")
			.to_owned();
		if object_format != "sha256" {
			return Err(RepositoryError::UnsupportedFormat(format!(
				"objectformat = {object_format} (only sha256 is supported)"
			)));
		}

		let version = config
			.get_int("core", None, "repositoryformatversion")
			.map_err(|error| RepositoryError::UnsupportedFormat(error.to_string()))?
			.unwrap_or(0);
		if version != 1 {
			return Err(RepositoryError::UnsupportedFormat(format!(
				"repositoryformatversion = {version} (sha256 requires 1)"
			)));
		}

		Ok(Config {
			repository_format_version: version as u32,
			object_format,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn round_trips_sha256_config() {
		let parsed = Config::parse(&Config::sha256().render()).expect("parse");
		assert_eq!(parsed, Config::sha256());
	}

	#[test]
	fn refuses_sha1_repo() {
		let text = "[core]\n\trepositoryformatversion = 0\n";
		assert!(matches!(
			Config::parse(text),
			Err(RepositoryError::UnsupportedFormat(_))
		));
	}

	#[test]
	fn reads_config_with_comments_and_quoting() {
		// git's own style, with a comment and other sections present.
		let text = "# gitana repo\n\
            [core]\n\
            \trepositoryformatversion = 1 ; version\n\
            \tbare = false\n\
            [extensions]\n\
            \tobjectformat = sha256\n";
		assert_eq!(Config::parse(text).unwrap(), Config::sha256());
	}
}
