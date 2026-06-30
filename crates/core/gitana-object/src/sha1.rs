use ::sha1::{Digest, Sha1 as Sha1Hasher};

use crate::hash_algorithm::HashAlgorithm;

/// The SHA-1 object-hash algorithm (20-byte ids). Marker type for [`crate::ObjectId`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Sha1;

impl HashAlgorithm for Sha1 {
	type Output = [u8; 20];

	const NAME: &'static str = "sha1";
	const RAW_LEN: usize = 20;
	const GPGSIG_HEADER: &'static str = "gpgsig";

	fn digest(parts: &[&[u8]]) -> Self::Output {
		let mut hasher = Sha1Hasher::new();
		for part in parts {
			hasher.update(part);
		}
		hasher.finalize().into()
	}
}
