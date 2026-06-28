/// Errors from git-config parsing and value interpretation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
	/// The config text was syntactically invalid.
	#[error("config parse error: {0}")]
	Parse(String),
	/// A value could not be interpreted as a boolean.
	#[error("not a boolean: {0:?}")]
	NotBool(String),
	/// A value could not be interpreted as an integer.
	#[error("not an integer: {0:?}")]
	NotInt(String),
}
