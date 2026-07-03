//! Minimal single-threaded executor for the sync WASI 0.2 export boundary.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

/// How many consecutive `Pending` polls to tolerate before declaring the future stuck.
const MAX_PENDING_POLLS: u64 = 1_000_000;

/// Drive a future to completion on the single wasm thread.
///
/// This is sound here because every `.await` in the engine bottoms out in the file
/// store's inline wasm `blocking()` helper (synchronous descriptor calls) or an
/// uncontended single-task mutex — nothing ever waits on a `wasi:io` pollable, so a
/// woken-by-nobody `Pending` is unreachable in practice. If that invariant is ever
/// broken (e.g. a future `wasi:http` transport), the bail-out below turns a silent
/// spin into a trap with a message.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
	let waker = Waker::noop();
	let mut cx = Context::from_waker(waker);
	let mut future = pin!(future);
	let mut pending_polls: u64 = 0;
	loop {
		match future.as_mut().poll(&mut cx) {
			Poll::Ready(value) => return value,
			Poll::Pending => {
				pending_polls += 1;
				assert!(
					pending_polls < MAX_PENDING_POLLS,
					"future is stuck Pending: nothing can wake it (no reactor on wasm)"
				);
				std::hint::spin_loop();
			}
		}
	}
}
