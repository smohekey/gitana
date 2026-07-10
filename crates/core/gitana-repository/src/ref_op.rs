use gitana_object::{HashAlgorithm, ObjectId};

use crate::ReflogIntent;

/// One ref mutation in a [`RefStore`](crate::RefStore) transaction.
///
/// A create/update (`new` set) or a delete (`new` unset), with the compare-and-set precondition
/// `expected` and the reflog intent. A transaction locks every op's ref (and `HEAD` for a split-HEAD
/// reflog cascade), validates every precondition, then commits reflogs and refs — so the ops apply
/// atomically, git's ref-lock transaction model (see `docs/hlds/ref-transactions.md`).
pub struct RefOp<'a, H: HashAlgorithm> {
	/// The ref name (`refs/heads/main`, `refs/tags/v1`, …).
	pub name: String,
	/// The required current value: `Some(id)` must match the current resolved value, `None` requires
	/// the ref to be absent.
	pub expected: Option<ObjectId<H>>,
	/// The new value: `Some(id)` creates or moves the ref, `None` deletes it.
	pub new: Option<ObjectId<H>>,
	/// Whether to append a reflog entry, and with what identity/message — gated by
	/// `core.logAllRefUpdates` at commit, exactly as a direct [`RefStore`](crate::RefStore) move is.
	pub reflog: ReflogIntent<'a>,
}
