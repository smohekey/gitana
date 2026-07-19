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
	/// Include expansion recursed deeper than git's maximum of 10 (also how a cycle is broken).
	#[error("exceeded maximum include depth (10)")]
	IncludeDepthExceeded,
	/// An include directive that would be processed has a bare `path` key (no value). git treats this
	/// as fatal (`missing value for 'include.path'`).
	#[error("missing value for 'include.path'")]
	IncludeMissingValue,
	/// A matched include path begins with `~/` but no `$HOME` is available to expand it. git treats
	/// this as fatal (`could not expand include path`).
	#[error("could not expand include path")]
	IncludeTildeNoHome,
	/// A matched include path uses the `~user/` form, which needs a passwd lookup this I/O-free,
	/// wasm-pure crate cannot perform. Deferred to the native driver (a later slice); fail-closed
	/// here rather than silently mis-resolve it as a relative path.
	#[error("unsupported '~user/' in include path")]
	IncludeUserTildeUnsupported,
}
