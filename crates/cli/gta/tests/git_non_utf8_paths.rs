#![cfg(all(unix, not(target_os = "macos")))]

use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::Path, process::Command};

#[test]
fn add_and_ls_files_preserve_native_bytes_from_a_non_utf8_subdirectory() {
	let repo = tempfile::tempdir().unwrap();
	let init = Command::new("git")
		.args(["init", "-q"])
		.arg(repo.path())
		.output()
		.expect("run git init");
	assert!(
		init.status.success(),
		"git init failed: {}",
		String::from_utf8_lossy(&init.stderr)
	);

	let directory = OsString::from_vec(b"sub-\xfe".to_vec());
	let file = OsString::from_vec(b"file-\xff".to_vec());
	let subdirectory = repo.path().join(&directory);
	std::fs::create_dir(&subdirectory).unwrap();
	std::fs::write(subdirectory.join(&file), b"raw\n").unwrap();

	let add = gta(&subdirectory)
		.arg("add")
		.arg(&file)
		.output()
		.expect("run gta add");
	assert!(
		add.status.success(),
		"gta add failed: {}",
		String::from_utf8_lossy(&add.stderr)
	);

	let listed = gta(repo.path())
		.args(["ls-files", "--full-name", "-z"])
		.output()
		.expect("run gta ls-files");
	assert!(
		listed.status.success(),
		"gta ls-files failed: {}",
		String::from_utf8_lossy(&listed.stderr)
	);
	assert_eq!(listed.stdout, b"sub-\xfe/file-\xff\0");
}

#[test]
fn standard_exclude_files_match_raw_path_bytes() {
	let repo = tempfile::tempdir().unwrap();
	let init = Command::new("git")
		.args(["init", "-q"])
		.arg(repo.path())
		.output()
		.expect("run git init");
	assert!(init.status.success());

	let info_name = OsString::from_vec(b"info-\xff".to_vec());
	let global_name = OsString::from_vec(b"global-\xfe".to_vec());
	std::fs::write(repo.path().join(&info_name), b"raw\n").unwrap();
	std::fs::write(repo.path().join(&global_name), b"raw\n").unwrap();
	std::fs::write(repo.path().join(".git/info/exclude"), b"info-\xff\n").unwrap();
	let global_excludes = repo.path().join(".git/global-excludes");
	std::fs::write(&global_excludes, b"global-\xfe\n").unwrap();
	let config = Command::new("git")
		.arg("-C")
		.arg(repo.path())
		.args(["config", "core.excludesFile"])
		.arg(&global_excludes)
		.output()
		.expect("configure core.excludesFile");
	assert!(config.status.success());

	let listed = gta(repo.path())
		.args(["ls-files", "-o", "--exclude-standard", "-z"])
		.output()
		.expect("run gta ls-files");
	assert!(
		listed.status.success(),
		"gta ls-files failed: {}",
		String::from_utf8_lossy(&listed.stderr)
	);
	assert!(
		listed.stdout.is_empty(),
		"raw-byte excludes must hide both files"
	);

	let status = gta(repo.path())
		.arg("status")
		.output()
		.expect("run gta status");
	assert!(
		status.status.success(),
		"gta status failed: {}",
		String::from_utf8_lossy(&status.stderr)
	);
	assert!(
		status.stdout.is_empty(),
		"raw-byte excludes must hide both files"
	);

	for path in [&info_name, &global_name] {
		let add = gta(repo.path())
			.arg("add")
			.arg(path)
			.output()
			.expect("run gta add");
		assert!(
			!add.status.success(),
			"an explicitly added raw-byte excluded path must be refused"
		);
	}
}

#[test]
fn sparse_reapply_matches_raw_pattern_bytes() {
	let repo = raw_sparse_repo();
	let raw_directory = OsString::from_vec(b"raw-\xff".to_vec());
	configure_sparse(repo.path());
	std::fs::write(
		repo.path().join(".git/info/sparse-checkout"),
		b"/*\n!/*/\n/raw-\xff/\n",
	)
	.unwrap();

	let reapply = gta(repo.path())
		.args(["sparse-checkout", "reapply"])
		.output()
		.expect("run sparse-checkout reapply");
	assert!(
		reapply.status.success(),
		"reapply failed: {}",
		String::from_utf8_lossy(&reapply.stderr)
	);
	assert!(repo.path().join(&raw_directory).join("file").exists());
	assert!(!repo.path().join("other/file").exists());
}

#[test]
fn cone_set_preserves_a_raw_invocation_prefix() {
	let repo = raw_sparse_repo();
	let raw_directory = OsString::from_vec(b"raw-\xff".to_vec());
	let raw_path = repo.path().join(&raw_directory);

	let set = gta(&raw_path)
		.args(["sparse-checkout", "set", "."])
		.output()
		.expect("run sparse-checkout set from raw directory");
	assert!(
		set.status.success(),
		"set failed: {}",
		String::from_utf8_lossy(&set.stderr)
	);
	assert!(raw_path.join("file").exists());
	assert!(!repo.path().join("other/file").exists());
	assert_eq!(
		std::fs::read(repo.path().join(".git/info/sparse-checkout")).unwrap(),
		b"/*\n!/*/\n/raw-\xff/\n"
	);
	let listed = gta(repo.path())
		.args(["sparse-checkout", "list"])
		.output()
		.unwrap();
	assert_eq!(listed.stdout, b"raw-\xff\n");
}

#[test]
fn sparse_set_and_add_accept_raw_arguments() {
	let repo = raw_sparse_repo();
	let raw_directory = OsString::from_vec(b"raw-\xff".to_vec());

	let set = gta(repo.path())
		.args(["sparse-checkout", "set"])
		.arg(&raw_directory)
		.output()
		.expect("run cone sparse-checkout set with raw argument");
	assert!(
		set.status.success(),
		"cone set failed: {}",
		String::from_utf8_lossy(&set.stderr)
	);
	assert_eq!(
		std::fs::read(repo.path().join(".git/info/sparse-checkout")).unwrap(),
		b"/*\n!/*/\n/raw-\xff/\n"
	);

	let raw_pattern = OsString::from_vec(b"raw-\xff/**".to_vec());
	let set = gta(repo.path())
		.args(["sparse-checkout", "set", "--no-cone"])
		.arg(&raw_pattern)
		.output()
		.expect("run non-cone sparse-checkout set with raw pattern");
	assert!(
		set.status.success(),
		"non-cone set failed: {}",
		String::from_utf8_lossy(&set.stderr)
	);
	assert_eq!(
		std::fs::read(repo.path().join(".git/info/sparse-checkout")).unwrap(),
		b"raw-\xff/**\n"
	);
	let listed = gta(repo.path())
		.args(["sparse-checkout", "list"])
		.output()
		.expect("list raw sparse pattern");
	assert_eq!(listed.stdout, b"raw-\xff/**\n");
}

#[test]
fn revision_suffix_preserves_raw_path_bytes() {
	let repo = raw_sparse_repo();
	let spec = OsString::from_vec(b"HEAD:raw-\xff/file".to_vec());

	let git = Command::new("git")
		.arg("-C")
		.arg(repo.path())
		.arg("rev-parse")
		.arg(&spec)
		.output()
		.expect("run git rev-parse with raw suffix");
	assert!(git.status.success());
	let resolved = gta(repo.path())
		.arg("rev-parse")
		.arg(&spec)
		.output()
		.expect("run gta rev-parse with raw suffix");
	assert!(
		resolved.status.success(),
		"gta rev-parse failed: {}",
		String::from_utf8_lossy(&resolved.stderr)
	);
	assert_eq!(resolved.stdout, git.stdout);

	let content = gta(repo.path())
		.args(["cat-file", "-p"])
		.arg(&spec)
		.output()
		.expect("run gta cat-file with raw suffix");
	assert!(
		content.status.success(),
		"gta cat-file failed: {}",
		String::from_utf8_lossy(&content.stderr)
	);
	assert_eq!(content.stdout, b"raw\n");

	let index_spec = OsString::from_vec(b":raw-\xff/file".to_vec());
	let staged = gta(repo.path())
		.args(["cat-file", "-p"])
		.arg(&index_spec)
		.output()
		.expect("run gta cat-file with raw index suffix");
	assert!(
		staged.status.success(),
		"gta cat-file index lookup failed: {}",
		String::from_utf8_lossy(&staged.stderr)
	);
	assert_eq!(staged.stdout, b"raw\n");
}

#[test]
fn checkout_restore_and_reset_accept_raw_revision_suffixes() {
	let repo = raw_sparse_repo();
	let spec = OsString::from_vec(b"HEAD:raw-\xff".to_vec());

	let checkout = gta(repo.path())
		.arg("checkout")
		.arg(&spec)
		.args(["--", "file"])
		.output()
		.expect("run gta checkout with raw tree suffix");
	assert!(
		checkout.status.success(),
		"gta checkout failed: {}",
		String::from_utf8_lossy(&checkout.stderr)
	);
	assert_eq!(std::fs::read(repo.path().join("file")).unwrap(), b"raw\n");

	std::fs::write(repo.path().join("file"), b"changed\n").unwrap();
	let restore = gta(repo.path())
		.arg("restore")
		.arg("--source")
		.arg(&spec)
		.arg("file")
		.output()
		.expect("run gta restore with raw tree suffix");
	assert!(
		restore.status.success(),
		"gta restore failed: {}",
		String::from_utf8_lossy(&restore.stderr)
	);
	assert_eq!(std::fs::read(repo.path().join("file")).unwrap(), b"raw\n");

	let head_before = Command::new("git")
		.arg("-C")
		.arg(repo.path())
		.args(["rev-parse", "HEAD"])
		.output()
		.expect("read HEAD before path reset");
	assert!(head_before.status.success());
	std::fs::write(repo.path().join("file"), b"staged\n").unwrap();
	let add = Command::new("git")
		.arg("-C")
		.arg(repo.path())
		.args(["add", "file"])
		.output()
		.expect("stage root file before path reset");
	assert!(add.status.success());
	let reset = gta(repo.path())
		.arg("reset")
		.arg(&spec)
		.args(["--", "file"])
		.output()
		.expect("run gta reset with raw tree suffix");
	assert!(
		reset.status.success(),
		"gta reset failed: {}",
		String::from_utf8_lossy(&reset.stderr)
	);
	let staged = Command::new("git")
		.arg("-C")
		.arg(repo.path())
		.args(["cat-file", "-p", ":file"])
		.output()
		.expect("read reset index entry");
	assert!(staged.status.success());
	assert_eq!(staged.stdout, b"raw\n");
	assert_eq!(
		std::fs::read(repo.path().join("file")).unwrap(),
		b"staged\n"
	);
	let head_after = Command::new("git")
		.arg("-C")
		.arg(repo.path())
		.args(["rev-parse", "HEAD"])
		.output()
		.expect("read HEAD after path reset");
	assert!(head_after.status.success());
	assert_eq!(head_after.stdout, head_before.stdout);
}

fn raw_sparse_repo() -> tempfile::TempDir {
	let repo = tempfile::tempdir().unwrap();
	let init = Command::new("git")
		.args(["init", "-q"])
		.arg(repo.path())
		.output()
		.unwrap();
	assert!(init.status.success());
	let raw_directory = OsString::from_vec(b"raw-\xff".to_vec());
	std::fs::create_dir(repo.path().join(&raw_directory)).unwrap();
	std::fs::write(repo.path().join(&raw_directory).join("file"), b"raw\n").unwrap();
	std::fs::create_dir(repo.path().join("other")).unwrap();
	std::fs::write(repo.path().join("other/file"), b"other\n").unwrap();
	let add = Command::new("git")
		.arg("-C")
		.arg(repo.path())
		.args(["add", "."])
		.output()
		.unwrap();
	assert!(add.status.success());
	let commit = Command::new("git")
		.arg("-C")
		.arg(repo.path())
		.args([
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			"base",
		])
		.output()
		.unwrap();
	assert!(commit.status.success());
	repo
}

fn configure_sparse(repo: &Path) {
	for (key, value) in [
		("core.sparseCheckout", "true"),
		("core.sparseCheckoutCone", "true"),
	] {
		let configured = Command::new("git")
			.arg("-C")
			.arg(repo)
			.args(["config", key, value])
			.output()
			.unwrap();
		assert!(configured.status.success());
	}
}

fn gta(directory: &Path) -> assert_cmd::Command {
	let mut command = assert_cmd::Command::cargo_bin("gta").unwrap();
	command.arg("-C").arg(directory);
	command
}
