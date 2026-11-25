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

	/// Parse a git config and check the repository's format; see [`Config::parse_bytes`].
	pub fn parse(text: &str) -> Result<Self, RepositoryError> {
		Self::parse_bytes(text.as_bytes())
	}

	/// Parse git-config bytes and check the repository's format as git does when it opens a repository
	/// (measured against git 2.55), refusing as well what gitana can't read or write correctly. `gta`, the
	/// `gitana:repo` wasm component and the linked-worktree library check a repository through this before
	/// using it; `Repository::new` itself doesn't.
	///
	/// - **`core.repositoryformatversion`** is validated at every occurrence as a 32-bit git `int` (git aborts
	///   on a malformed or out-of-range one even when a later valid value shadows it), and the effective version
	///   must be at most 1.
	/// - **Extensions git knows** (the `EXTENSIONS` table) have every occurrence's value checked, whatever the
	///   version: booleans must be git booleans, and the others must have a value, from a fixed set where
	///   git has one (case-sensitive, as git's are).
	/// - **At version 1** every extension applies, and a name git doesn't know is refused. A config
	///   subsection is part of the name: `[extensions "noop"] x` is the unknown `noop.x`.
	/// - **At version 0** the extensions git honours there apply, a version-1-only one is refused, and an
	///   unknown name is ignored.
	/// - **With no version, or a negative one**, git applies no extension at all (`refStorage = reftable`
	///   there is an ordinary files repository to git), so neither does this.
	/// - **gitana refuses, at version 1:** `refStorage = reftable` (only the files ref backend is
	///   implemented), `compatObjectFormat` (no compatibility object format is kept), and
	///   `submodulePathConfig = true` (submodule git directories are only looked for under `modules/`).
	pub fn parse_bytes(bytes: &[u8]) -> Result<Self, RepositoryError> {
		let config = GitConfigBytes::parse(bytes).map_err(|error| unsupported(error.to_string()))?;

		let version = config
			.get_i32_validated("core", None, "repositoryformatversion")
			.map_err(|error| unsupported(format!("invalid core.repositoryformatversion: {error}")))?;
		if let Some(version) = version
			&& version > 1
		{
			return Err(unsupported(format!(
				"repository format version {version} (git reads versions up to 1)"
			)));
		}
		check_extensions(&config, version)?;

		let object_format = match version {
			Some(1) => match config.get_raw("extensions", None, "objectformat") {
				Some(Some(value)) if value == b"sha256" => "sha256",
				_ => "sha1",
			},
			_ => "sha1",
		};
		Ok(Config {
			repository_format_version: if version == Some(1) { 1 } else { 0 },
			object_format: object_format.to_owned(),
		})
	}
}

/// A repository extension git knows: whether git honours it at repository format version 0, and what
/// its value must be.
struct Extension {
	name: &'static str,
	at_version_0: bool,
	value: ExtensionValue,
}

enum ExtensionValue {
	/// Anything, or nothing.
	Any,
	/// A git boolean.
	Bool,
	/// A value must be given; when the list isn't empty, one of these (case-sensitive).
	Required(&'static [&'static str]),
}

/// The extensions git 2.55 knows (measured: any other name is fatal to it at version 1).
const EXTENSIONS: &[Extension] = &[
	Extension {
		name: "noop",
		at_version_0: true,
		value: ExtensionValue::Any,
	},
	Extension {
		name: "noop-v1",
		at_version_0: false,
		value: ExtensionValue::Any,
	},
	Extension {
		name: "preciousobjects",
		at_version_0: true,
		value: ExtensionValue::Bool,
	},
	Extension {
		name: "partialclone",
		at_version_0: true,
		value: ExtensionValue::Required(&[]),
	},
	Extension {
		name: "worktreeconfig",
		at_version_0: true,
		value: ExtensionValue::Bool,
	},
	Extension {
		name: "objectformat",
		at_version_0: false,
		value: ExtensionValue::Required(&["sha1", "sha256"]),
	},
	Extension {
		name: "compatobjectformat",
		at_version_0: false,
		value: ExtensionValue::Required(&[]),
	},
	Extension {
		name: "refstorage",
		at_version_0: false,
		value: ExtensionValue::Required(&["files", "reftable"]),
	},
	Extension {
		name: "relativeworktrees",
		at_version_0: false,
		value: ExtensionValue::Bool,
	},
	Extension {
		name: "submodulepathconfig",
		at_version_0: false,
		value: ExtensionValue::Bool,
	},
];

/// The extension rules of [`Config::parse_bytes`], given the effective format version.
fn check_extensions(config: &GitConfigBytes, version: Option<i32>) -> Result<(), RepositoryError> {
	for extension in EXTENSIONS {
		let name = extension.name;
		match extension.value {
			ExtensionValue::Any => {}
			ExtensionValue::Bool => {
				config
					.get_bool_validated("extensions", None, name)
					.map_err(|error| unsupported(format!("invalid value for extensions.{name}: {error}")))?;
			}
			ExtensionValue::Required(allowed) => {
				for value in config.get_all_raw("extensions", None, name) {
					let Some(value) = value else {
						return Err(unsupported(format!("missing value for extensions.{name}")));
					};
					if !allowed.is_empty() && !allowed.iter().any(|allowed| value == allowed.as_bytes()) {
						return Err(unsupported(format!(
							"invalid value for extensions.{name}: {}",
							String::from_utf8_lossy(&value)
						)));
					}
				}
			}
		}
	}

	if !matches!(version, Some(0 | 1)) {
		return Ok(());
	}
	for (key, _) in config.entries() {
		let Some(name) = key.strip_prefix(b"extensions.") else {
			continue;
		};
		match EXTENSIONS
			.iter()
			.find(|known| name.eq_ignore_ascii_case(known.name.as_bytes()))
		{
			Some(known) if version == Some(0) && !known.at_version_0 => {
				return Err(unsupported(format!(
					"extensions.{} needs repository format version 1, found 0",
					known.name
				)));
			}
			Some(_) => {}
			None if version == Some(1) => {
				return Err(unsupported(format!(
					"unknown extension extensions.{}",
					String::from_utf8_lossy(name)
				)));
			}
			None => {}
		}
	}

	if version == Some(1) {
		if config.get_raw("extensions", None, "refstorage") == Some(Some(b"reftable".to_vec())) {
			return Err(unsupported(
				"extensions.refstorage = reftable: gitana supports only the files ref backend so far"
					.to_owned(),
			));
		}
		if config
			.get_raw("extensions", None, "compatobjectformat")
			.is_some()
		{
			return Err(unsupported(
				"extensions.compatobjectformat: gitana keeps no compatibility object format".to_owned(),
			));
		}
		if config
			.get_bool("extensions", None, "submodulepathconfig")
			.ok()
			.flatten()
			== Some(true)
		{
			return Err(unsupported(
				"extensions.submodulepathconfig: gitana looks for submodule git directories only under modules/"
					.to_owned(),
			));
		}
	}
	Ok(())
}

fn unsupported(message: String) -> RepositoryError {
	RepositoryError::UnsupportedFormat(message)
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

	/// A config with `core.repositoryformatversion = version` (none when `None`) and `extensions` lines.
	fn with_extensions(version: Option<&str>, extensions: &str) -> String {
		let version = version.map_or(String::new(), |v| {
			format!("\trepositoryformatversion = {v}\n")
		});
		format!("[core]\n{version}\tbare = false\n[extensions]\n\t{extensions}\n")
	}

	/// git 2.55's verdict on a repository's format version and extensions (each measured with
	/// `git rev-parse --git-dir` on a repository with that config), and gitana's own refusals on top.
	/// `Some(format)`: accepted, with that object format; `None`: refused.
	#[test]
	fn checks_the_repository_format_as_git_does() {
		let v1 = Some("1");
		let v0 = Some("0");
		let cases: &[(Option<&str>, &str, Option<&str>)] = &[
			// Version 1: every extension applies; an unknown name is refused.
			(v1, "noop = anything", Some("sha1")),
			(v1, "noop-v1 = anything", Some("sha1")),
			(v1, "objectFormat = sha1", Some("sha1")),
			(v1, "objectFormat = sha256", Some("sha256")),
			(v1, "refStorage = files", Some("sha1")),
			(v1, "REFSTORAGE = files", Some("sha1")),
			(v1, "worktreeConfig = true", Some("sha1")),
			(v1, "preciousObjects = true", Some("sha1")),
			(v1, "relativeWorktrees = true", Some("sha1")),
			(v1, "partialClone = origin", Some("sha1")),
			(v1, "submodulePathConfig = false", Some("sha1")),
			(v1, "objectFormat = sha512", None),
			(v1, "objectFormat", None),
			(v1, "objectFormat = bogus\n\tobjectFormat = sha1", None),
			(v1, "refStorage = bogus", None),
			(v1, "refStorage = Files", None),
			(v1, "refStorage", None),
			(v1, "partialClone", None),
			(v1, "worktreeConfig = notabool", None),
			(v1, "preciousObjects = notabool", None),
			(v1, "relativeWorktrees = notabool", None),
			(v1, "submodulePathConfig = notabool", None),
			(v1, "made-up-name = true", None),
			// gitana refuses these; git accepts them (`compatObjectFormat` only in a git built with Rust).
			(v1, "refStorage = reftable", None),
			(v1, "compatObjectFormat = sha256", None),
			(v1, "submodulePathConfig = true", None),
			// Version 0: the extensions git honours there apply, a version-1-only one is refused, and an
			// unknown name is ignored; values are still checked.
			(v0, "noop = true", Some("sha1")),
			(v0, "preciousObjects = true", Some("sha1")),
			(v0, "partialClone = origin", Some("sha1")),
			(v0, "worktreeConfig = true", Some("sha1")),
			(v0, "made-up-name = true", Some("sha1")),
			(v0, "objectFormat = sha1", None),
			(v0, "objectFormat = sha256", None),
			(v0, "compatObjectFormat = sha256", None),
			(v0, "refStorage = files", None),
			(v0, "refStorage = reftable", None),
			(v0, "relativeWorktrees = true", None),
			(v0, "submodulePathConfig = true", None),
			(v0, "noop-v1 = true", None),
			(v0, "refStorage = bogus", None),
			(v0, "worktreeConfig = notabool", None),
			// No version, or a negative one: no extension applies, but values are still checked.
			(None, "refStorage = reftable", Some("sha1")),
			(Some("-1"), "refStorage = reftable", Some("sha1")),
			(Some("-1"), "objectFormat = sha256", Some("sha1")),
			(Some("-5"), "noop-v1 = x", Some("sha1")),
			(Some("-1"), "made-up-name = true", Some("sha1")),
			(Some("-1"), "refStorage = bogus", None),
			(None, "worktreeConfig = notabool", None),
			// The version itself.
			(Some("2"), "noop = true", None),
			(Some("1x"), "noop = true", None),
			// git reads the version as a 32-bit `int`, units and all.
			(Some("-2147483648"), "noop = true", Some("sha1")),
			(Some("-1k"), "noop = true", Some("sha1")),
			(Some("-2147483649"), "noop = true", None),
			(Some("2147483648"), "noop = true", None),
		];
		for (version, extensions, expected) in cases {
			let text = with_extensions(*version, extensions);
			let parsed = Config::parse(&text);
			assert_eq!(
				parsed
					.as_ref()
					.ok()
					.map(|config| config.object_format.as_str()),
				*expected,
				"version {version:?}, {extensions:?}: got {parsed:?}"
			);
			if expected.is_none() {
				assert!(
					matches!(parsed, Err(RepositoryError::UnsupportedFormat(_))),
					"version {version:?}, {extensions:?}: got {parsed:?}"
				);
			}
		}
	}

	#[test]
	fn a_config_subsection_is_part_of_the_extension_name() {
		// `[extensions "noop"] x` is the unknown extension `noop.x` to git, not `noop`.
		let text = "[core]\n\trepositoryformatversion = 1\n[extensions \"noop\"]\n\tx = 1\n";
		assert!(matches!(
			Config::parse(text),
			Err(RepositoryError::UnsupportedFormat(_))
		));
	}

	#[test]
	fn every_occurrence_of_the_version_is_validated() {
		// git aborts on a malformed version even when a later valid one shadows it.
		let text = "[core]\n\trepositoryformatversion = 1x\n\trepositoryformatversion = 1\n";
		assert!(matches!(
			Config::parse(text),
			Err(RepositoryError::UnsupportedFormat(_))
		));
	}

	#[test]
	fn the_reftable_refusal_names_it() {
		let error = Config::parse(&with_extensions(Some("1"), "refStorage = reftable")).unwrap_err();
		assert!(
			error
				.to_string()
				.contains("extensions.refstorage = reftable"),
			"{error}"
		);
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
