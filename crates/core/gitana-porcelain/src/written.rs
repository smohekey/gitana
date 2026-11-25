//! A commit written from another: what concluding a cherry-pick or a revert returns.

use gitana_object::{HashAlgorithm, ObjectId};

/// A commit written from another (the commit a cherry-pick or a revert concludes with), and whether
/// the text it carried from that one had to be repaired: bytes that aren't UTF-8, read as Latin-1
/// ([`DecodedCommit::repaired`](gitana_object::DecodedCommit::repaired)). git warns when it writes
/// such a commit ("commit message did not conform to UTF-8"), so a caller can too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written<H: HashAlgorithm> {
	/// The commit written.
	pub commit: ObjectId<H>,
	/// Whether the text it carried was repaired.
	pub repaired: bool,
}
