//! `gta` OpenPGP signing end-to-end against stock git+GnuPG: a `gta commit -S` / `gta tag -s` object
//! produced under `gpg.format=openpgp` verifies with `git verify-commit` / `git tag -v` — the reverse
//! of the verify-side interop (does *other people's* git accept what gitana's gpg signer produces).
//!
//! Everything runs in an isolated GnuPG home so nothing global is touched; a `gpg.program` wrapper
//! points both gitana and git at that home (which also exercises the `gpg.program` override). Needs a
//! SHA-256-capable git and `gpg`; both are probed and skipped.
//!
//! Unix-only: it drives signing through a bash `gpg.program` wrapper and sets file modes.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A SHA-256 repo configured to sign with an ephemeral GnuPG key via a `gpg.program` wrapper that
/// pins an isolated `GNUPGHOME` and non-interactive (loopback, empty-passphrase) signing. Returns the
/// work dir.
fn gpg_repo(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	let gnupg = work.join("gnupg");
	std::fs::create_dir_all(&gnupg).unwrap();
	set_perms(&gnupg);
	std::fs::write(gnupg.join("gpg-agent.conf"), "allow-loopback-pinentry\n").unwrap();
	std::fs::write(gnupg.join("gpg.conf"), "pinentry-mode loopback\n").unwrap();

	// An ed25519 signing key with no passphrase, in the isolated home.
	gpg(
		&gnupg,
		&[
			"--batch",
			"--pinentry-mode",
			"loopback",
			"--passphrase",
			"",
			"--quick-generate-key",
			"Gitana Test <t@e>",
			"ed25519",
			"sign",
			"0",
		],
	);
	let fpr = gpg_fingerprint(&gnupg);

	// A wrapper so both gitana and git run gpg non-interactively against the isolated home — and so the
	// `gpg.program` config is exercised, not just the default `gpg`.
	let wrapper = work.join("gpg-wrap.sh");
	std::fs::write(
		&wrapper,
		format!(
			"#!/usr/bin/env bash\nexport GNUPGHOME={home}\nexec gpg --batch --pinentry-mode loopback --passphrase '' \"$@\"\n",
			home = gnupg.to_str().unwrap()
		),
	)
	.unwrap();
	set_executable(&wrapper);

	git(w, &["init", "--object-format=sha256", "-q", "."]);
	git(w, &["config", "user.name", "Gitana Test"]);
	git(w, &["config", "user.email", "t@e"]);
	git(w, &["config", "gpg.format", "openpgp"]);
	git(w, &["config", "gpg.program", wrapper.to_str().unwrap()]);
	git(w, &["config", "user.signingkey", &fpr]);

	std::fs::write(work.join("a.txt"), b"hello\n").unwrap();
	work
}

#[test]
fn gpg_signed_commit_verifies_with_git() {
	if skip() {
		return;
	}
	let work = gpg_repo("gta-gpg-commit");
	let w = work.to_str().unwrap();

	gta(w, &["add", "."]);
	gta(w, &["commit", "-S", "-m", "openpgp signed"]);

	// git accepts gitana's OpenPGP-signed commit — the interop guarantee. (`git verify-commit` prints
	// its status to stderr.)
	let out = git_status(&["-C", w, "verify-commit", "HEAD"]);
	assert!(
		out.contains("Good signature"),
		"git did not verify the gta gpg-signed commit: {out}"
	);
}

#[test]
fn gpg_signed_tag_verifies_with_git() {
	if skip() {
		return;
	}
	let work = gpg_repo("gta-gpg-tag");
	let w = work.to_str().unwrap();

	gta(w, &["add", "."]);
	gta(w, &["commit", "-m", "base"]);
	gta(w, &["tag", "-s", "v1", "-m", "release"]);

	let out = git_status(&["-C", w, "tag", "-v", "v1"]);
	assert!(
		out.contains("Good signature"),
		"git did not verify the gta gpg-signed tag: {out}"
	);
}

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

fn git(dir: &str, args: &[&str]) {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// Run `git <args>` and return its combined stderr (where verify status is printed), asserting success.
fn git_status(args: &[&str]) -> String {
	let out = Command::new("git").args(args).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8_lossy(&out.stderr).into_owned()
}

fn gpg(home: &Path, args: &[&str]) {
	let out = Command::new("gpg")
		.arg("--homedir")
		.arg(home)
		.args(args)
		.output()
		.expect("run gpg");
	assert!(
		out.status.success(),
		"gpg {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// The fingerprint of the (single) key in `home`, uppercase hex.
fn gpg_fingerprint(home: &Path) -> String {
	let out = Command::new("gpg")
		.arg("--homedir")
		.arg(home)
		.args(["--list-keys", "--with-colons"])
		.output()
		.expect("run gpg --list-keys");
	String::from_utf8_lossy(&out.stdout)
		.lines()
		.find_map(|line| line.strip_prefix("fpr:"))
		.map(|rest| rest.trim_matches(':').to_owned())
		.expect("a key fingerprint")
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

#[cfg(unix)]
fn set_perms(dir: &Path) {
	use std::os::unix::fs::PermissionsExt;
	std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
fn set_executable(path: &Path) {
	use std::os::unix::fs::PermissionsExt;
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn skip() -> bool {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return true;
	}
	if !have_gpg() {
		eprintln!("skipping: gpg not available");
		return true;
	}
	false
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-gpg");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}

/// Whether gpg can actually generate a signing key here — probed, because some sandboxes ship the
/// `gpg` binary but no working `gpg-agent` (key generation then fails), and the test should skip
/// rather than fail where it cannot run. Cached.
fn have_gpg() -> bool {
	use std::sync::OnceLock;
	static USABLE: OnceLock<bool> = OnceLock::new();
	*USABLE.get_or_init(|| {
		let probe = unique_tmp("probe-gpg");
		let home = probe.join("gnupg");
		if std::fs::create_dir_all(&home).is_err() {
			return false;
		}
		set_perms(&home);
		let _ = std::fs::write(home.join("gpg-agent.conf"), "allow-loopback-pinentry\n");
		let _ = std::fs::write(home.join("gpg.conf"), "pinentry-mode loopback\n");
		let ok = Command::new("gpg")
			.arg("--homedir")
			.arg(&home)
			.args([
				"--batch",
				"--pinentry-mode",
				"loopback",
				"--passphrase",
				"",
				"--quick-generate-key",
				"probe <p@x>",
				"ed25519",
				"sign",
				"0",
			])
			.output()
			.map(|out| out.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
