//! A minimal proof that gitana's object-database layer runs as a WASI component.
//!
//! It builds a [`LocalFileStore`] over a confined root (a host preopen under
//! `wasm32-wasip2`, an ambient-opened temp dir natively), wraps it in an
//! [`ObjectStore`], and round-trips one blob object — write then read back, checking
//! the id and bytes. Run natively with `cargo run -p wasm-object-db`, or as a component:
//!
//! ```sh
//! cargo build -p wasm-object-db --target wasm32-wasip2
//! wasmtime run --dir=/tmp/gitana-store::/store \
//!   target/wasm32-wasip2/debug/wasm-object-db.wasm
//! ```

use anyhow::Result;
use gitana_file_store_local::LocalFileStore;
use gitana_object::{ObjectKind, Sha256};
use gitana_object_store::ObjectStore;

fn main() -> Result<()> {
	let objects = ObjectStore::<_, Sha256>::new(open_store());

	let payload = b"hello, gitana object db on wasi\n";
	let id = block_on(objects.write_object(ObjectKind::Blob, payload))?;
	let (kind, bytes) = block_on(objects.read_object(&id))?;

	assert_eq!(kind, ObjectKind::Blob);
	assert_eq!(bytes, payload);
	println!("round-trip ok: {} ({} bytes)", id.to_hex(), bytes.len());
	Ok(())
}

/// The store's confined root. Under `wasm32-wasip2` the host preopens `/store`; the
/// demo takes that preopen's *descriptor* — the same capability handle a component
/// export would be passed — rather than going through the preopen path table.
#[cfg(target_arch = "wasm32")]
fn open_store() -> LocalFileStore {
	let dir = wasip2::filesystem::preopens::get_directories()
		.into_iter()
		.find_map(|(descriptor, path)| (path == "/store").then_some(descriptor))
		.expect("host must preopen /store (wasmtime run --dir=<host>::/store)");
	LocalFileStore::from_descriptor(dir)
}

#[cfg(not(target_arch = "wasm32"))]
fn open_store() -> LocalFileStore {
	let root = std::env::temp_dir().join("gitana-wasm-object-db-demo");
	std::fs::create_dir_all(&root).expect("create demo store root");
	LocalFileStore::from_dir(
		cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())
			.expect("open store root"),
	)
}

/// Drive a future to completion. Natively the store offloads blocking filesystem work via
/// `spawn_blocking`, which needs a Tokio runtime; on wasm the filesystem layer runs inline
/// and the future never suspends on a reactor, so a no-op waker poll loop suffices — no
/// executor dependency and no thread parking (which wasm lacks).
#[cfg(not(target_arch = "wasm32"))]
fn block_on<F: std::future::Future>(future: F) -> F::Output {
	tokio::runtime::Builder::new_current_thread()
		.build()
		.expect("build tokio runtime")
		.block_on(future)
}

#[cfg(target_arch = "wasm32")]
fn block_on<F: std::future::Future>(future: F) -> F::Output {
	let waker = std::task::Waker::noop();
	let mut cx = std::task::Context::from_waker(waker);
	let mut future = std::pin::pin!(future);
	loop {
		match future.as_mut().poll(&mut cx) {
			std::task::Poll::Ready(value) => return value,
			std::task::Poll::Pending => std::hint::spin_loop(),
		}
	}
}
