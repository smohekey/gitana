use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::hash_algorithm::HashAlgorithm;

/// The SHA-256 object-hash algorithm (32-byte ids). Marker type for [`crate::ObjectId`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Sha256;

impl HashAlgorithm for Sha256 {
	type Output = [u8; 32];

	const NAME: &'static str = "sha256";
	const RAW_LEN: usize = 32;
	const GPGSIG_HEADER: &'static str = "gpgsig-sha256";

	fn digest(parts: &[&[u8]]) -> Self::Output {
		let mut hasher = Sha256Hasher::new();
		for part in parts {
			hasher.update(part);
		}
		hasher.finalize().into()
	}
}
