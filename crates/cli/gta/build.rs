use std::env;

fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
		return;
	}

	// clap's generated parser and debug async command frames can exceed the default 1 MiB.
	// Reserve virtual address space for this executable; leave the commit size on demand.
	let argument = if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
		"/STACK:8388608"
	} else {
		"-Wl,--stack,8388608"
	};
	println!("cargo:rustc-link-arg-bin=gta={argument}");
}
