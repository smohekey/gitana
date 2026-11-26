use std::path::{Path, PathBuf};

/// The spawn this guard looks for, built so this file doesn't contain it.
const RAW_SPAWN: &str = concat!("Command::new(", "\"git\")");

/// Panic, listing each `file:line`, if a Rust file under `crate_dir`'s `src/` or `tests/` spawns git
/// directly (`Command::new` with `"git"`) instead of through [`git_command`](crate::git_command).
///
/// A crate that has moved onto `git_command` calls this from one test with `env!("CARGO_MANIFEST_DIR")`,
/// so a new test there that spawns git on its own fails until it uses the helper. Comment lines are
/// skipped. A spawn spelled differently (a variable for the program, or spaces inside the call) isn't
/// caught.
pub fn assert_git_spawned_only_through_git_command(crate_dir: &Path) {
	let mut found = Vec::new();
	for dir in ["src", "tests"] {
		scan(&crate_dir.join(dir), crate_dir, &mut found);
	}
	assert!(
		found.is_empty(),
		"these spawn git directly instead of through gitana_test_git::git_command(): {found:#?}"
	);
}

/// Every non-comment line under `dir` (recursively, `.rs` files only) that contains [`RAW_SPAWN`], as
/// `path:line` relative to `root`.
fn scan(dir: &Path, root: &Path, found: &mut Vec<String>) {
	let Ok(entries) = std::fs::read_dir(dir) else {
		return;
	};
	let mut paths: Vec<PathBuf> = entries
		.filter_map(|entry| entry.ok().map(|entry| entry.path()))
		.collect();
	paths.sort();
	for path in paths {
		if path.is_dir() {
			scan(&path, root, found);
		} else if path.extension().is_some_and(|extension| extension == "rs") {
			let text = std::fs::read_to_string(&path).unwrap_or_default();
			for (index, line) in text.lines().enumerate() {
				if line.contains(RAW_SPAWN) && !line.trim_start().starts_with("//") {
					let shown = path.strip_prefix(root).unwrap_or(&path);
					found.push(format!("{}:{}", shown.display(), index + 1));
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{RAW_SPAWN, assert_git_spawned_only_through_git_command};

	/// A fresh crate-shaped directory with `files` (relative path, contents).
	fn crate_with(tag: &str, files: &[(&str, String)]) -> std::path::PathBuf {
		let root = std::env::temp_dir().join(format!(
			"gitana-test-git-guard-{tag}-{}",
			std::process::id()
		));
		let _ = std::fs::remove_dir_all(&root);
		for (relative, contents) in files {
			let path = root.join(relative);
			std::fs::create_dir_all(path.parent().unwrap()).unwrap();
			std::fs::write(path, contents).unwrap();
		}
		root
	}

	fn guard_message(root: &Path) -> Option<String> {
		std::panic::catch_unwind(|| assert_git_spawned_only_through_git_command(root))
			.err()
			.map(|panic| panic.downcast_ref::<String>().cloned().unwrap_or_default())
	}

	#[test]
	fn names_each_direct_spawn() {
		let root = crate_with(
			"direct",
			&[
				("src/lib.rs", "fn a() {}\n".to_owned()),
				(
					"tests/a.rs",
					format!("fn b() {{\n\tlet _ = std::process::{RAW_SPAWN};\n}}\n"),
				),
				(
					"tests/support/mod.rs",
					format!("fn c() {{ {RAW_SPAWN}; }}\n"),
				),
			],
		);
		let message = guard_message(&root).expect("the guard fails");
		assert!(message.contains("tests/a.rs:2"), "{message}");
		assert!(message.contains("tests/support/mod.rs:1"), "{message}");
		assert!(!message.contains("src/lib.rs"), "{message}");
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn passes_the_helper_and_comments() {
		let root = crate_with(
			"clean",
			&[
				(
					"src/lib.rs",
					format!("// tests used to call {RAW_SPAWN} here\nfn a() {{}}\n"),
				),
				(
					"tests/a.rs",
					"fn b() { let _ = gitana_test_git::git_command(); }\n".to_owned(),
				),
			],
		);
		assert_eq!(guard_message(&root), None);
		let _ = std::fs::remove_dir_all(&root);
	}
}
