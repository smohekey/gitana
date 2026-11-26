//! Waiting in tests for another task to reach a point, bounded by time rather than by a count.

use std::time::{Duration, Instant};

/// Yield until `done` says so or ten seconds pass. A count of yields gave up too soon on a busy
/// machine (under load on macOS, the other task hadn't got there within 100,000), so the wait is by
/// time; the caller then asserts what it waited for.
pub(crate) async fn yield_until(mut done: impl FnMut() -> bool) {
	let deadline = Instant::now() + Duration::from_secs(10);
	while !done() && Instant::now() < deadline {
		tokio::task::yield_now().await;
	}
}
