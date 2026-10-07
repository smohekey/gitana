/// A precondition asserted under a ref transaction's held locks, before any op applies.
///
/// Passed to [`RefStore::transact_checked`](crate::RefStore::transact_checked). Each precondition is
/// checked while the transaction holds its full lock set, so no concurrent update can interleave
/// between the check and the commit; a failed precondition applies nothing.
/// [`RefStore::transact`](crate::RefStore::transact) asserts no preconditions.
///
/// `#[non_exhaustive]` so further precondition kinds can be added without a breaking change.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum Precondition {
	/// `HEAD` must still symbolically name this ref by its immediate target, e.g.
	/// `"refs/heads/main"`.
	///
	/// Asserted while `HEAD.lock` is held, so a concurrent `set_symbolic` retarget cannot interleave
	/// before the transaction commits. The immediate (single-hop) target is compared, matching how a
	/// caller captures it with one [`RefStore::read_symbolic`](crate::RefStore::read_symbolic);
	/// multi-hop symbolic `HEAD` is out of scope. A mismatch fails the transaction with
	/// [`RepositoryError::HeadMoved`](crate::RepositoryError::HeadMoved).
	HeadSymbolic(String),
}
