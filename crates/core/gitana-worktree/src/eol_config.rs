use gitana_config::GitConfig;

use crate::WorktreeError;

/// The config that governs line endings, read as git reads it: `core.autocrlf`, `core.eol` and
/// `core.safecrlf`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EolConfig {
	pub(crate) autocrlf: AutoCrlf,
	pub(crate) eol: CoreEol,
	pub(crate) safecrlf: SafeCrlf,
}

/// `core.autocrlf`: `input`, or a boolean (unset is false).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutoCrlf {
	False,
	True,
	Input,
}

/// `core.eol`: `lf`, `crlf` or `native`; any other value, as unset, counts as git's default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoreEol {
	Unset,
	Lf,
	Crlf,
}

/// `core.safecrlf`: `warn` (the default), or a boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeCrlf {
	False,
	Warn,
	True,
}

impl EolConfig {
	/// Read from `config`. An invalid boolean in `core.autocrlf` or `core.safecrlf` is an error,
	/// as git's is; an unknown `core.eol` counts as unset, as git takes it.
	pub(crate) fn read(config: &GitConfig) -> Result<EolConfig, WorktreeError> {
		let is = |name: &str, word: &str| {
			config
				.get_string("core", None, name)
				.is_some_and(|value| value.eq_ignore_ascii_case(word))
		};
		let autocrlf = if is("autocrlf", "input") {
			AutoCrlf::Input
		} else if config.get_bool("core", None, "autocrlf")? == Some(true) {
			AutoCrlf::True
		} else {
			AutoCrlf::False
		};
		let eol = if is("eol", "lf") {
			CoreEol::Lf
		} else if is("eol", "crlf") {
			CoreEol::Crlf
		} else if is("eol", "native") {
			if cfg!(windows) {
				CoreEol::Crlf
			} else {
				CoreEol::Lf
			}
		} else {
			CoreEol::Unset
		};
		let safecrlf = if is("safecrlf", "warn") {
			SafeCrlf::Warn
		} else {
			match config.get_bool("core", None, "safecrlf")? {
				Some(true) => SafeCrlf::True,
				Some(false) => SafeCrlf::False,
				None => SafeCrlf::Warn,
			}
		};
		Ok(EolConfig {
			autocrlf,
			eol,
			safecrlf,
		})
	}

	/// git's `text_eol_is_crlf`: whether text files are written with CRLF.
	pub(crate) fn text_eol_is_crlf(&self) -> bool {
		match self.autocrlf {
			AutoCrlf::True => true,
			AutoCrlf::Input => false,
			AutoCrlf::False => match self.eol {
				CoreEol::Crlf => true,
				CoreEol::Lf => false,
				CoreEol::Unset => cfg!(windows),
			},
		}
	}
}
