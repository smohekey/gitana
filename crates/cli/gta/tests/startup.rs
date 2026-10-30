//! Startup must work before repository discovery, including on Windows' main thread.

#[test]
fn version_runs_outside_a_repository() {
	let temporary = tempfile::tempdir().unwrap();

	let output = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(temporary.path())
		.arg("--version")
		.output()
		.unwrap();

	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(
		String::from_utf8(output.stdout).unwrap().trim(),
		concat!("gta ", env!("CARGO_PKG_VERSION"))
	);
}
