use gitana_object::{HashAlgorithm, ObjectId};

/// How a tree is reconciled into the working tree and index.
pub(crate) enum CheckoutMode<H: HashAlgorithm> {
	/// git `read-tree --reset -u`: the working tree and index become the target authoritatively, discarding
	/// any local (staged or unstaged) divergence. Backs `reset --hard`, `switch --force`, and the internal
	/// force checkouts (rebase original-tree restore, clone materialise).
	Reset,
	/// The historical non-force checkout: materialise the target and refuse to clobber a *dirty* tracked
	/// file, but treat the target as authoritative (removes index entries absent from it). Preserved for the
	/// porcelain checkouts pending their two-tree-merge migration.
	Overlay,
	/// git `read-tree -m -u` from `head`: apply only the `head`→target diff, preserving non-conflicting local
	/// (staged/unstaged) divergences and refusing conflicting ones. Backs `switch` — so staged work git would
	/// carry across a branch switch is not silently discarded.
	Merge { head: ObjectId<H> },
}
