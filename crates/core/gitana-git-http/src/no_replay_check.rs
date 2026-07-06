use std::convert::Infallible;
use std::future::Future;

use crate::NonceLedger;

/// The no-op [`NonceLedger`]: never reports a replay and records nothing. The default for a host that
/// does not enforce one-time nonces (accepting replay within the freshness window, v1's documented
/// behaviour). [`verify_push`](crate::verify_push) uses it.
pub struct NoReplayCheck;

impl NonceLedger for NoReplayCheck {
	type Error = Infallible;

	fn check_and_record(
		&self,
		_nonce: &str,
		_expires_at: u64,
	) -> impl Future<Output = Result<bool, Infallible>> {
		std::future::ready(Ok(false))
	}
}
