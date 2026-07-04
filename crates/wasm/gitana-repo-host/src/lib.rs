//! Native wasmtime host harness for the `gitana-repo-component` guest.
//!
//! Instantiates the component with **no preopens**: the only filesystem authority the
//! guest ever receives is the directory descriptor the host mints explicitly
//! ([`grant_dir`]) and passes to `repository.open`.
//!
//! The mirror image of the guest crate's gating: the host is native-only, so on wasm
//! targets this crate compiles to nothing and workspace-wide wasm checks stay green.

#![cfg(not(target_arch = "wasm32"))]

use std::path::Path;

use anyhow::Result;
use wasmtime::component::{Component, Linker, Resource};
use wasmtime::error::Context as _;
use wasmtime::{Engine, Store};
use wasmtime_wasi::filesystem::{Descriptor, Dir};
use wasmtime_wasi::{
	DirPerms, FilePerms, OpenMode, ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};

// wasmtime-wasi constructs its `Dir` from the cap-std major it was built against (3.x),
// while the gitana workspace is on cap-std 4.x — the two must not be conflated, so the
// host names its own compatible cap-std under a distinct import.
use cap_std_host::ambient_authority;

wasmtime::component::bindgen!({
	path: "../gitana-repo-component/wit",
	world: "repo",
	imports: { default: async },
	exports: { default: async },
	with: {
		// `wasi:http` is satisfied by wasmtime-wasi-http's own host bindings (longest-prefix wins over
		// the general `wasi` mapping below), so the guest's outgoing-handler import is host-mediated.
		"wasi:http": wasmtime_wasi_http::p2::bindings::http,
		"wasi": wasmtime_wasi::p2::bindings,
	},
});

/// Store state: the WASI context (no preopens), the `wasi:http` context, and the resource
/// table descriptors are minted into.
pub struct State {
	ctx: WasiCtx,
	http_ctx: WasiHttpCtx,
	table: ResourceTable,
}

impl WasiView for State {
	fn ctx(&mut self) -> WasiCtxView<'_> {
		WasiCtxView {
			ctx: &mut self.ctx,
			table: &mut self.table,
		}
	}
}

impl WasiHttpView for State {
	fn http(&mut self) -> WasiHttpCtxView<'_> {
		WasiHttpCtxView {
			ctx: &mut self.http_ctx,
			table: &mut self.table,
			hooks: Default::default(),
		}
	}
}

/// A default engine (component-model async support is always on in wasmtime 46).
pub fn engine() -> Result<Engine> {
	Ok(Engine::new(&wasmtime::Config::new())?)
}

/// A store whose WASI context grants **nothing**: no preopens, no args, no env.
/// (stderr is inherited so a guest panic is visible when a test fails.)
pub fn store(engine: &Engine) -> Store<State> {
	let ctx = WasiCtxBuilder::new().inherit_stderr().build();
	Store::new(
		engine,
		State {
			ctx,
			http_ctx: WasiHttpCtx::new(),
			table: ResourceTable::new(),
		},
	)
}

/// Instantiate the component at `component_path` against the p2 WASI linker.
pub async fn instantiate(
	engine: &Engine,
	store: &mut Store<State>,
	component_path: &Path,
) -> Result<Repo> {
	let component = Component::from_file(engine, component_path)
		.with_context(|| format!("loading component {}", component_path.display()))?;
	let mut linker = Linker::new(engine);
	wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
	wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
	Ok(Repo::instantiate_async(store, &component, &linker).await?)
}

/// Mint a `wasi:filesystem` directory descriptor for `host_path` and push it into the
/// store's resource table — the capability subsequently handed to `repository.open`.
/// This is the host-side edge where ambient authority is exercised, on the host's
/// behalf, exactly once per granted directory.
pub fn grant_dir(store: &mut Store<State>, host_path: &Path) -> Result<Resource<Descriptor>> {
	let dir = cap_std_host::fs::Dir::open_ambient_dir(host_path, ambient_authority())
		.with_context(|| format!("opening {}", host_path.display()))?;
	let dir = Dir::new(
		dir,
		DirPerms::all(),
		FilePerms::all(),
		OpenMode::READ | OpenMode::WRITE,
		false,
	);
	Ok(store.data_mut().table.push(Descriptor::Dir(dir))?)
}
