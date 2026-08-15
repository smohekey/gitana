use gitana_object::{HashAlgorithm, ObjectId};

/// How a tree is reconciled into the working tree and index.
pub(crate) enum CheckoutMode<H: HashAlgorithm> {
	/// git `read-tree --reset -u`: the working tree and index become the target authoritatively, discarding
	/// any local (staged or unstaged) divergence. Backs `reset --hard`, `switch --force`, and the internal
	/// force checkouts (rebase original-tree restore, clone materialise).
	Reset,
	/// The *headless* non-force checkout: materialise the target and refuse to clobber a *dirty* tracked or
	/// in-the-way untracked file, treating the target as authoritative (removes index entries absent from
	/// it). Used where there is no prior "from" tree to drive a two-tree merge — the wasm component's
	/// `checkout` and a fresh linked-worktree's initial materialise. (The porcelain merge-like checkouts —
	/// cherry-pick / revert / merge / rebase — moved onto `Merge`, which they can because their index still
	/// equals HEAD at checkout time.)
	Overlay,
	/// git `read-tree -m -u` from `head`: apply only the `head`→target diff, preserving non-conflicting local
	/// (staged/unstaged) divergences and refusing conflicting ones. Backs `switch` — so staged work git would
	/// carry across a branch switch is not silently discarded.
	Merge { head: ObjectId<H> },
}
