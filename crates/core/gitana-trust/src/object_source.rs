use std::future::Future;

use gitana_object::{HashAlgorithm, ObjectId, ObjectKind};

/// Read access to the repository's object store, the one capability trust-root folding needs. Kept
/// as a minimal trait (rather than a dependency on `gitana-repository`) so `gitana-trust` stays a
/// pure library — unit-testable against an in-memory map and usable in a wasm component. The
/// repository/receive-pack layers implement it over their object store.
pub trait ObjectSource<H: HashAlgorithm> {
	/// The backend's read error. Surfaced through [`crate::TrustError::ObjectSource`].
	type Error: std::error::Error + Send + Sync + 'static;

	/// Read the object `id`, returning its kind and raw (decoded) payload — the same
	/// `(kind, bytes)` a loose/packed object store yields.
	fn read_object(
		&self,
		id: &ObjectId<H>,
	) -> impl Future<Output = Result<(ObjectKind, Vec<u8>), Self::Error>>;
}
