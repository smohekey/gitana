//! A `wasm32-wasip2` reactor component exposing gitana repository operations.
//!
//! The component's exports receive their filesystem access as an owned
//! `wasi:filesystem` directory *descriptor* — a capability handed over by the
//! host — never a preopen-path convention or ambient authority. On native
//! targets this crate compiles to an empty library so the workspace builds
//! uniformly.

/// Bindings for the `repo` world. The `with` remaps are the type-unification
/// linchpin: the world's imported `wasi:filesystem/types.descriptor` *is*
/// [`wasip2::filesystem::types::Descriptor`] — the same concrete type
/// `LocalFileStore::from_descriptor` takes — so a host-granted handle flows from
/// the export straight into the file store with no bridging.
#[cfg(target_arch = "wasm32")]
mod bindings {
	wit_bindgen::generate!({
		path: "wit",
		world: "repo",
		with: {
			"wasi:filesystem/types@0.2.12": wasip2::filesystem::types,
			"wasi:io/error@0.2.12": wasip2::io::error,
			"wasi:io/poll@0.2.12": wasip2::io::poll,
			"wasi:io/streams@0.2.12": wasip2::io::streams,
			"wasi:clocks/wall-clock@0.2.12": wasip2::clocks::wall_clock,
		},
	});
}

#[cfg(target_arch = "wasm32")]
mod block_on;
#[cfg(target_arch = "wasm32")]
mod component;
#[cfg(target_arch = "wasm32")]
mod inner;
#[cfg(target_arch = "wasm32")]
mod ops;

#[cfg(target_arch = "wasm32")]
use self::component::Component;

#[cfg(target_arch = "wasm32")]
bindings::export!(Component with_types_in bindings);
