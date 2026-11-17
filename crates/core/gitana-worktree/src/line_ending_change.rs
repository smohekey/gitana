/// Which way a file's line endings change between the working tree and the repository, as git's
/// `core.safecrlf` reports it: staging turns CRLF into LF, or the next checkout turns LF into CRLF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEndingChange {
	/// The file's CRLF line endings are stored as LF, and won't be restored by a checkout.
	CrlfToLf,
	/// The file's LF line endings will be written as CRLF by the next checkout.
	LfToCrlf,
}
