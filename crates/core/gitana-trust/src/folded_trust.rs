use crate::{KeyId, TrustRoot};

/// A folded trust chain: its effective [`TrustRoot`] together with the [`KeyId`] that signed the
/// chain's bootstrap (root) commit — the *anchor* the whole chain's authority rests on.
///
/// The anchor is what makes adopting a never-before-seen root safe. A key merely *listed* in a root
/// is public and can be copied into an attacker's forged chain, so pinning a listed fingerprint
/// proves nothing; but a chain's bootstrap commit is self-signed, and no one can produce that
/// signature without the anchor key's private half. Callers that adopt an unseen root (trust
/// bootstrap-on-first-use) pin the anchor, not a listed key.
#[derive(Debug, Clone)]
pub struct FoldedTrust {
	/// The effective trust root at the folded chain's tip.
	pub root: TrustRoot,
	/// The key that signed the chain's bootstrap commit.
	pub anchor: KeyId,
}
