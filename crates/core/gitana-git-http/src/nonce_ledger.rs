use std::future::Future;

/// A host-supplied record of push-certificate nonces already used, so a *replayed* nonce — one that
/// is still fresh but has been seen before — can be rejected. v1's nonce is a stateless HMAC, so
/// replay within the freshness window is otherwise accepted (a documented trade-off); a host that
/// wants to close that window supplies a ledger. The core stays pure — the state lives in the host
/// (an in-memory map for a single instance, a shared TTL cache across instances).
///
/// [`NoReplayCheck`](crate::NoReplayCheck) is the no-op default for a host that does not want replay
/// protection; [`verify_push`](crate::verify_push) uses it, and
/// [`verify_push_with_ledger`](crate::verify_push_with_ledger) takes a real one.
pub trait NonceLedger {
	/// The host lookup's error. Surfaced through [`GitHttpError::NonceLedger`](crate::GitHttpError).
	type Error: std::error::Error + Send + Sync + 'static;

	/// Record `nonce` as used and report whether it had **already** been recorded (i.e. this is a
	/// replay). `expires_at` is a unix-time hint after which the entry may be evicted — the nonce is no
	/// longer fresh past it, so keeping it no longer prevents a replay. Must be atomic: two concurrent
	/// pushes of the same nonce cannot both see `false`.
	fn check_and_record(
		&self,
		nonce: &str,
		expires_at: u64,
	) -> impl Future<Output = Result<bool, Self::Error>>;
}
