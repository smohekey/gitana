fn main() {
	// In-guest pack encoding materializes miniz_oxide's ~300 KiB compressor state on
	// the (shadow) stack in debug builds, deep inside the repack call chain — more
	// than the wasm default 1 MiB stack. Native threads get 8 MiB; match that.
	// (wasm-ld only; the native build of this crate is an empty library.)
	let target = std::env::var("TARGET").unwrap_or_default();
	if target.starts_with("wasm32-wasi") {
		println!("cargo:rustc-link-arg-cdylib=-zstack-size=8388608");
	}
}
