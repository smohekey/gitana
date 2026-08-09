use gitana_config::{GitConfig, GitConfigBytes};
use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::HashAlgorithm;

use crate::RepositoryError;

/// The repository identity the engine cares about, read from / written to the git
/// `config` file via [`GitConfig`]: the object-hash format and the format version.
///
/// gitana understands `sha1` (git's classic format, `repositoryformatversion = 0`, no
/// `extensions.objectformat`) and `sha256` (`repositoryformatversion = 1` plus
/// `extensions.objectformat = sha256`). [`Config::parse`] refuses any other format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
	/// `core.repositoryformatversion` (0 for sha1, 1 for sha256).
	pub repository_format_version: u32,
	/// `extensions.objectformat` (`sha1` / `sha256`).
	pub object_format: String,
}

impl Config {
	/// The config gitana writes on `init` for the hash algorithm `H`.
	pub fn for_algorithm<H: HashAlgorithm>() -> Self {
		Config {
			// git records sha256 as repositoryformatversion 1 (extensions are read); sha1 is the
			// classic version-0 layout with no objectformat extension.
			repository_format_version: if H::NAME == "sha256" { 1 } else { 0 },
			object_format: H::NAME.to_owned(),
		}
	}

	/// The config gitana writes for a sha256 repository.
	pub fn sha256() -> Self {
		Config {
			repository_format_version: 1,
			object_format: "sha256".to_owned(),
		}
	}

	/// The config gitana writes for a (classic) sha1 repository.
	pub fn sha1() -> Self {
		Config {
			repository_format_version: 0,
			object_format: "sha1".to_owned(),
		}
	}

	/// Render to git config text. A sha256 repo carries `extensions.objectformat`; a sha1
	/// repo (version 0) omits it, exactly as stock git writes each format.
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
		// Only a version-1 (sha256) repo declares the object-format extension; a version-0 sha1
		// repo has none, and writing one at version 0 would not be honoured by git.
		if self.repository_format_version >= 1 {
			set(
				&mut config,
				"extensions",
				None,
				"objectformat",
				&self.object_format,
			);
		}
		config.render()
	}

	/// Read and validate the repository config from `store` (the git directory's file store).
	/// Unrelated non-UTF-8 values do not affect the ASCII repository-format keys.
	pub async fn read(store: &impl FileStore) -> Result<Self, RepositoryError> {
		let bytes = match store.read_path("config").await {
			Ok(bytes) => bytes,
			Err(FileStoreError::NotFound) => {
				return Err(RepositoryError::UnsupportedFormat(
					"no config file".to_owned(),
				));
			}
			Err(other) => return Err(other.into()),
		};
		Self::parse_bytes(&bytes)
	}

	/// Parse a git config and validate the repository is a supported format (`sha1` or
	/// `sha256`), with a `repositoryformatversion` consistent with that format.
	pub fn parse(text: &str) -> Result<Self, RepositoryError> {
		Self::parse_bytes(text.as_bytes())
	}

	/// Parse arbitrary git-config bytes and validate the supported repository format.
	pub fn parse_bytes(bytes: &[u8]) -> Result<Self, RepositoryError> {
		let config = GitConfigBytes::parse(bytes)
			.map_err(|error| RepositoryError::UnsupportedFormat(error.to_string()))?;

		// git treats an absent `extensions.objectformat` as sha1.
		let object_format = config
			.get_raw("extensions", None, "objectformat")
			.flatten()
			.unwrap_or_else(|| b"sha1".to_vec());
		let object_format = std::str::from_utf8(&object_format)
			.map_err(|_| RepositoryError::UnsupportedFormat("objectformat is not UTF-8".to_owned()))?;

		let version = config
			.get_int("core", None, "repositoryformatversion")
			.map_err(|error| RepositoryError::UnsupportedFormat(error.to_string()))?
			.unwrap_or(0);

		let version_ok = match object_format {
			// git records sha256 only at version 1 (where extensions are read).
			"sha256" => version == 1,
			// sha1 is the classic version-0 layout, but git also accepts a version-1 repo with an
			// explicit `objectformat = sha1`, so allow either.
			"sha1" => version == 0 || version == 1,
			other => {
				return Err(RepositoryError::UnsupportedFormat(format!(
					"objectformat = {other} (only sha1 and sha256 are supported)"
				)));
			}
		};
		if !version_ok {
			return Err(RepositoryError::UnsupportedFormat(format!(
				"repositoryformatversion = {version} is invalid for objectformat = {object_format}"
			)));
		}

		Ok(Config {
			repository_format_version: version as u32,
			object_format: object_format.to_owned(),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use gitana_object::{Sha1, Sha256};

	#[test]
	fn round_trips_sha256_config() {
		let parsed = Config::parse(&Config::sha256().render()).expect("parse");
		assert_eq!(parsed, Config::sha256());
	}

	#[test]
	fn round_trips_sha1_config() {
		let parsed = Config::parse(&Config::sha1().render()).expect("parse");
		assert_eq!(parsed, Config::sha1());
		// A classic sha1 repo is version 0 with no objectformat extension.
		assert!(!Config::sha1().render().contains("objectformat"));
	}

	#[test]
	fn for_algorithm_matches_the_named_constructors() {
		assert_eq!(Config::for_algorithm::<Sha256>(), Config::sha256());
		assert_eq!(Config::for_algorithm::<Sha1>(), Config::sha1());
	}

	#[test]
	fn accepts_a_classic_sha1_repo() {
		let text = "[core]\n\trepositoryformatversion = 0\n";
		assert_eq!(Config::parse(text).unwrap(), Config::sha1());
	}

	#[test]
	fn accepts_sha1_at_version_one() {
		// git also accepts a version-1 repo with an explicit `objectformat = sha1`.
		let text = "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha1\n";
		let parsed = Config::parse(text).unwrap();
		assert_eq!(parsed.object_format, "sha1");
		assert_eq!(parsed.repository_format_version, 1);
	}

	#[test]
	fn refuses_an_unknown_object_format() {
		let text = "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha999\n";
		assert!(matches!(
			Config::parse(text),
			Err(RepositoryError::UnsupportedFormat(_))
		));
	}

	#[test]
	fn refuses_sha256_at_the_wrong_version() {
		// objectformat = sha256 must be version 1; version 0 is inconsistent.
		let text = "[core]\n\trepositoryformatversion = 0\n[extensions]\n\tobjectformat = sha256\n";
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
