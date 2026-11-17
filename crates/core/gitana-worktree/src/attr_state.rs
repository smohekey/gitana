/// What an attribute is for a path, as git's `.gitattributes` gives it: set (`text`), unset
/// (`-text`), unspecified (`!text`, or no rule names it), or a value (`eol=crlf`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttrState {
	Set,
	Unset,
	Unspecified,
	Value(Vec<u8>),
}
