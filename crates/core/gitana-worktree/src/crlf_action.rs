/// What git does with a file's line endings (convert.c's `convert_crlf_action`, once
/// `convert_attrs` has settled it from the attributes and config).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrlfAction {
	/// Not text: left alone both ways.
	Binary,
	/// Text: CRLF stored as LF; written with LF.
	TextInput,
	/// Text: CRLF stored as LF; written with CRLF.
	TextCrlf,
	/// Text if it looks like text: stored as LF; written with LF.
	AutoInput,
	/// Text if it looks like text: stored as LF; written with CRLF.
	AutoCrlf,
}
