//! The removal policy: which semantics [`remove`](crate::remove) applies.

/// Which removal semantics [`remove`](crate::remove) applies to a [`RemoveRequest`](crate::RemoveRequest).
///
/// [`Conservative`](RemovePolicy::Conservative) is the default and the Code Henge stance — a safe, force-free
/// removal that refuses every unsafe state (dirty, locked, residual untracked/ignored, a primary or
/// identity-mismatched worktree) and preserves residual content. [`GitCompat`](RemovePolicy::GitCompat) is
/// git's `worktree remove` for a git-faithful CLI: `force` is the repeatable `-f` count. `force >= 1` takes a
/// separate **structural** path — like `git worktree remove -f`, it skips the *cleanliness* check (deleting a
/// dirty/untracked/ignored checkout) but still validates the `.git` **structure** the way git does (probed
/// against git 2.50.1): the checkout `.git` gitfile and admin cross-pointers must agree, and the admin `HEAD`
/// must **exist as a present file** — git does *not* validate HEAD *content* (an empty/garbage/padded/symref
/// HEAD is still removed), only its existence; a missing or directory HEAD, or a broken/reused checkout, is
/// refused, not deleted. The repository **format** is also validated before any destructive action (object
/// format, `repositoryformatversion`, and git's abort on an unknown `extensions.*`), so a repo gitana does not
/// fully understand is never force-mutated. `force >= 2` additionally removes a locked worktree. Identity,
/// primary, and enclosure are never overridden. `force 0` behaves as [`Conservative`](RemovePolicy::Conservative)
/// (its content gates still apply; the residual-content refusal is a deliberate, safe divergence from git,
/// which deletes ignored-only build artifacts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemovePolicy {
	/// Refuse every unsafe state and preserve residual content — the safe, force-free default (Code Henge).
	#[default]
	Conservative,
	/// git's `worktree remove` semantics, `force` being the repeatable `-f` count (`0` behaves as
	/// [`Conservative`](RemovePolicy::Conservative); `1` removes a dirty worktree; `2` also removes a locked
	/// one).
	GitCompat {
		/// The repeatable `-f` count.
		force: u8,
	},
}
