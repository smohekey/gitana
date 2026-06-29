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
	/// A plain `set` cannot overwrite a variable that already holds multiple values.
	#[error(
		"cannot overwrite multiple values of '{0}' with a single value; use --replace-all to replace them, or --add to append another"
	)]
	MultipleValues(String),
}
