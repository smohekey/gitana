//! `gta trust set-policy --dry-run` — the migration preflight (`docs/trust-migration.md`, step 8d):
//! it reports the cutover impact and writes nothing. Needs `ssh-keygen` to bootstrap the root.

use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn set_policy_require_dry_run_reports_and_changes_nothing() {
	if !have_ssh_keygen() {
		eprintln!("skipping: ssh-keygen not available");
		return;
	}
	let dir = unique_tmp("trust-preflight");
	let key = dir.join("id");
	ssh_keygen(&[
		"-t",
		"ed25519",
		"-N",
		"",
		"-C",
		"test",
		"-q",
		"-f",
		key.to_str().unwrap(),
	]);

	let repo = dir.join("repo");
	gta_init(&repo);
	let repo = repo.to_str().unwrap();
	let key = key.to_str().unwrap();
	gta(repo, &["config", "user.name", "T"]);
	gta(repo, &["config", "user.email", "t@x"]);
	gta(repo, &["config", "gpg.format", "ssh"]);
	// Bootstrap a `warn` root with a single key.
	gta(
		repo,
		&["trust", "init", "--policy", "warn", "--signing-key", key],
	);

	// Preview the flip to `require`: it reports, warns about the single key, and makes no change.
	let out = gta(
		repo,
		&[
			"trust",
			"set-policy",
			"require",
			"--signing-key",
			key,
			"--dry-run",
		],
	);
	assert!(out.contains("Dry run"), "{out}");
	assert!(out.contains("`warn` -> `require`"), "{out}");
	assert!(
		out.contains("the real command needs `--break-glass`"),
		"{out}"
	);
	assert!(out.contains("No changes made"), "{out}");

	// The same preview with `--break-glass` reflects the override: it would proceed, not block.
	let with_bg = gta(
		repo,
		&[
			"trust",
			"set-policy",
			"require",
			"--signing-key",
			key,
			"--break-glass",
			"--dry-run",
		],
	);
	assert!(
		with_bg.contains("proceeding under `--break-glass`"),
		"{with_bg}"
	);
	assert!(
		!with_bg.contains("the real command needs"),
		"break-glass preview must not say the command needs it: {with_bg}"
	);

	// The policy is still `warn`: neither dry run wrote anything.
	let list = gta(repo, &["trust", "list"]);
	assert!(list.contains("policy: warn"), "{list}");

	std::fs::remove_dir_all(&dir).ok();
}

fn gta_init(repo: &Path) {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["init", repo.to_str().unwrap()])
		.output()
		.expect("run gta init");
	assert!(
		out.status.success(),
		"gta init failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// Run `gta -C dir <args>`, asserting success and returning stdout.
fn gta(dir: &str, args: &[&str]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.output()
		.expect("run gta");
	assert!(
		out.status.success(),
		"gta {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("gta stdout utf8")
}

fn ssh_keygen(args: &[&str]) {
	let out = Command::new("ssh-keygen")
		.args(args)
		.output()
		.expect("run ssh-keygen");
	assert!(
		out.status.success(),
		"ssh-keygen {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

fn have_ssh_keygen() -> bool {
	Command::new("ssh-keygen").arg("-?").output().is_ok()
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}
