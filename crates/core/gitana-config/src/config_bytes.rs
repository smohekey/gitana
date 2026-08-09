//! Read-only Git configuration over arbitrary bytes.

use crate::{ConfigError, GitConfig};

/// A parsed Git configuration file whose values retain their original bytes.
///
/// Git's configuration grammar uses ASCII syntax but permits arbitrary bytes in
/// values. [`GitConfig`] remains the editable UTF-8 model; this companion type
/// is for repository-local reads that must not reject an otherwise valid file
/// merely because an unrelated value is not UTF-8.
#[derive(Clone)]
pub struct GitConfigBytes {
	config: GitConfig,
}

impl GitConfigBytes {
	/// Parses one configuration file without requiring its contents to be UTF-8.
	pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
		let encoded = encode(bytes);
		GitConfig::parse(&encoded).map(|config| Self { config })
	}

	/// Returns the last direct value while distinguishing absent and valueless keys.
	pub fn get_raw(
		&self,
		section: &str,
		subsection: Option<&[u8]>,
		name: &str,
	) -> Option<Option<Vec<u8>>> {
		let subsection = subsection.map(encode);
		self
			.config
			.get_raw(section, subsection.as_deref(), name)
			.map(|value| value.map(decode))
	}

	/// Returns every direct value in file order, retaining valueless occurrences.
	pub fn get_all_raw(
		&self,
		section: &str,
		subsection: Option<&[u8]>,
		name: &str,
	) -> Vec<Option<Vec<u8>>> {
		let subsection = subsection.map(encode);
		self
			.config
			.get_all_raw(section, subsection.as_deref(), name)
			.into_iter()
			.map(|value| value.map(decode))
			.collect()
	}

	/// Interprets the effective value using Git's boolean grammar.
	pub fn get_bool(
		&self,
		section: &str,
		subsection: Option<&[u8]>,
		name: &str,
	) -> Result<Option<bool>, ConfigError> {
		let subsection = subsection.map(encode);
		self.config.get_bool(section, subsection.as_deref(), name)
	}

	/// Validates every occurrence as a boolean and returns the effective value.
	pub fn get_bool_validated(
		&self,
		section: &str,
		subsection: Option<&[u8]>,
		name: &str,
	) -> Result<Option<bool>, ConfigError> {
		let subsection = subsection.map(encode);
		self
			.config
			.get_bool_validated(section, subsection.as_deref(), name)
	}

	/// Interprets the effective value using Git's integer grammar.
	pub fn get_int(
		&self,
		section: &str,
		subsection: Option<&[u8]>,
		name: &str,
	) -> Result<Option<i64>, ConfigError> {
		let subsection = subsection.map(encode);
		self.config.get_int(section, subsection.as_deref(), name)
	}

	/// Validates every occurrence as an integer and returns the effective value.
	pub fn get_int_validated(
		&self,
		section: &str,
		subsection: Option<&[u8]>,
		name: &str,
	) -> Result<Option<i64>, ConfigError> {
		let subsection = subsection.map(encode);
		self
			.config
			.get_int_validated(section, subsection.as_deref(), name)
	}

	/// Returns every dotted key and value in file order.
	pub fn entries(&self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
		self
			.config
			.entries()
			.into_iter()
			.map(|(key, value)| (decode(&key), value.map(decode)))
			.collect()
	}
}

/// Reversibly maps each input byte to the Unicode scalar with the same value.
/// Git config syntax is ASCII, so the existing parser sees the same grammar
/// while arbitrary value bytes remain recoverable.
fn encode(bytes: &[u8]) -> String {
	bytes.iter().copied().map(char::from).collect()
}

fn decode(text: &str) -> Vec<u8> {
	text
		.chars()
		.map(|character| {
			u8::try_from(u32::from(character))
				.expect("byte-encoded config cannot produce a non-byte character")
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn arbitrary_value_bytes_and_last_value_are_preserved() {
		let config = GitConfigBytes::parse(
			b"[core]\n\tworktree\n\tworktree = /first\n\tworktree = \"/last\\t\xff\"\n",
		)
		.unwrap();

		assert_eq!(
			config.get_raw("core", None, "worktree"),
			Some(Some(b"/last\t\xff".to_vec()))
		);
		assert_eq!(
			config.get_all_raw("core", None, "worktree"),
			vec![
				None,
				Some(b"/first".to_vec()),
				Some(b"/last\t\xff".to_vec())
			]
		);
	}

	#[test]
	fn typed_values_and_entries_share_the_existing_grammar() {
		let config = GitConfigBytes::parse(
			b"[core]\n\tbare = false\n\trepositoryformatversion = 1\n[remote \"o\"]\n\turl = \xff\n",
		)
		.unwrap();

		assert_eq!(config.get_bool("core", None, "bare").unwrap(), Some(false));
		assert_eq!(
			config
				.get_int("core", None, "repositoryformatversion")
				.unwrap(),
			Some(1)
		);
		assert!(
			config
				.entries()
				.contains(&(b"remote.o.url".to_vec(), Some(vec![0xff])))
		);
	}
}
