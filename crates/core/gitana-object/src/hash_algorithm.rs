use std::fmt::Debug;
use std::hash::Hash;

/// A git object-hash algorithm, used as the type parameter `H` of [`crate::ObjectId`].
///
/// Implementors are zero-sized marker types (e.g. [`crate::Sha1`], [`crate::Sha256`]);
/// the trait's associated constants and [`digest`](HashAlgorithm::digest) describe the
/// algorithm's width, name, and digest. Repositories are single-algorithm, so a
/// concrete `H` is chosen at the crate boundary and threaded through the object model.
pub trait HashAlgorithm:
	Clone + Copy + PartialEq + Eq + Hash + PartialOrd + Ord + Debug + 'static
{
	/// The exact raw-digest storage for this algorithm: `[u8; 20]` for SHA-1, `[u8; 32]`
	/// for SHA-256. [`crate::ObjectId`] holds one of these, so an id is exactly as wide
	/// as its hash.
	type Output: AsRef<[u8]>
		+ AsMut<[u8]>
		+ Default
		+ Copy
		+ PartialEq
		+ Eq
		+ Hash
		+ PartialOrd
		+ Ord
		+ Debug;

	/// The git `extensions.objectformat` name (`"sha1"` / `"sha256"`).
	const NAME: &'static str;
	/// The raw digest width in bytes (20 / 32), i.e. the length of [`Output`](Self::Output).
	const RAW_LEN: usize;
	/// The commit signature header for this algorithm: bare `gpgsig` for SHA-1, and
	/// `gpgsig-sha256` for SHA-256 (git names the SHA-256 variant explicitly).
	const GPGSIG_HEADER: &'static str;

	/// Hash the concatenation of `parts` into the raw digest. (Named `digest` to avoid
	/// clashing with the [`Hash`](std::hash::Hash) supertrait's `hash` method.)
	fn digest(parts: &[&[u8]]) -> Self::Output;
}
