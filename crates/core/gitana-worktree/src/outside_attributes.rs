/// The attributes files git reads from outside the repository, as the frontend found them: their
/// contents, or `None` for a file that's absent (or not to be read). They rank below the
/// repository's own `.gitattributes` and `info/attributes`, the system file lowest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutsideAttributes {
	/// The system file: git's `/etc/gitattributes`, unless `GIT_ATTR_NOSYSTEM` is set.
	pub system: Option<Vec<u8>>,
	/// The global file: `core.attributesFile`, by default `$XDG_CONFIG_HOME/git/attributes` (or
	/// `~/.config/git/attributes`).
	pub global: Option<Vec<u8>>,
}
