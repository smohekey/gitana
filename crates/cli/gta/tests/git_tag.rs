//! `gta tag` end-to-end against stock git: a `gta tag -s` object verifies with `git tag -v`, an
//! annotated `-a` tag is a real tag object, a bare name stays lightweight, and git config
//! `tag.gpgSign` drives signing (with `--no-sign` overriding it). SSH signing needs `ssh-keygen`, and
//! opening gitana's SHA-256 repo with git needs a SHA-256-capable git — both are probed and skipped.

use std::path::PathBuf;
use std::process::Command;

/// A SHA-256 repo with one commit, an ephemeral SSH signing key configured (`gpg.format=ssh`,
/// `user.signingkey`), and an allowed-signers file so `git tag -v` can verify. Returns the work dir.
fn signed_repo(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	let w = work.to_str().unwrap();
	git(w, &["init", "--object-format=sha256", "-q", "."]);
	git(w, &["config", "user.name", "T"]);
	git(w, &["config", "user.email", "t@e"]);

	std::fs::write(work.join("a.txt"), b"hello\n").unwrap();
	git(w, &["add", "."]);
	git(w, &["commit", "-q", "-m", "first"]);

	// An ephemeral ed25519 key; `user.signingkey` points at its public half (gta derives nothing).
	ssh_keygen(&[
		"-q",
		"-t",
		"ed25519",
		"-N",
		"",
		"-C",
		"t@e",
		"-f",
		work.join("key").to_str().unwrap(),
	]);
	let pubkey = std::fs::read_to_string(work.join("key.pub")).unwrap();
	git(w, &["config", "gpg.format", "ssh"]);
	git(
		w,
		&[
			"config",
			"user.signingkey",
			work.join("key.pub").to_str().unwrap(),
		],
	);
	// git verifies an SSH signature against a principal (the tagger email) in this file.
	std::fs::write(work.join("allowed"), format!("t@e {pubkey}")).unwrap();
	git(
		w,
		&[
			"config",
			"gpg.ssh.allowedSignersFile",
			work.join("allowed").to_str().unwrap(),
		],
	);

	work
}

#[test]
fn signed_tag_verifies_with_git() {
	if skip() {
		return;
	}
	let work = signed_repo("gta-tag-signed");
	let w = work.to_str().unwrap();

	gta(w, &["tag", "-s", "v1", "-m", "release one"]);

	// git accepts the SSHSIG under the trusted key and namespace — the full interop guarantee.
	// (`git tag -v` prints its verification status to stderr.)
	let out = git_verify(w, "v1");
	assert!(
		out.contains("Good \"git\" signature for t@e"),
		"git did not verify the gta-signed tag: {out}"
	);
	// It is a real annotated tag object pointing at HEAD, with the message preserved.
	assert_eq!(git(w, &["cat-file", "-t", "v1"]).trim(), "tag");
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	let raw = git(w, &["cat-file", "tag", "v1"]);
	assert!(raw.contains(&format!("object {head}")), "{raw}");
	assert!(raw.contains("release one"), "{raw}");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_gpgsign_signs_an_annotated_tag() {
	if skip() {
		return;
	}
	let work = signed_repo("gta-tag-config");
	let w = work.to_str().unwrap();
	git(w, &["config", "tag.gpgSign", "true"]);

	// No `-s`: signing is driven by `tag.gpgSign`.
	gta(w, &["tag", "-a", "v1", "-m", "release"]);
	let out = git_verify(w, "v1");
	assert!(out.contains("Good \"git\" signature for t@e"), "{out}");

	// `--no-sign` overrides the config: an annotated but unsigned tag (`git tag -v` reports no signature).
	gta(w, &["tag", "-a", "v2", "--no-sign", "-m", "release"]);
	assert_eq!(git(w, &["cat-file", "-t", "v2"]).trim(), "tag");
	let raw = git(w, &["cat-file", "tag", "v2"]);
	assert!(
		!raw.contains("-----BEGIN SSH SIGNATURE-----"),
		"v2 was signed: {raw}"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn annotated_tag_is_a_tag_object_and_bare_name_stays_lightweight() {
	if skip() {
		return;
	}
	let work = signed_repo("gta-tag-kinds");
	let w = work.to_str().unwrap();
	let head = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	// `-m` implies `-a`: a tag object, unsigned (no signing was requested).
	gta(w, &["tag", "v-annot", "-m", "annotated"]);
	assert_eq!(git(w, &["cat-file", "-t", "v-annot"]).trim(), "tag");
	let raw = git(w, &["cat-file", "tag", "v-annot"]);
	assert!(raw.contains("annotated"), "{raw}");
	assert!(
		!raw.contains("-----BEGIN SSH SIGNATURE-----"),
		"unexpected signature: {raw}"
	);

	// A bare name is a lightweight tag: the ref resolves straight to the commit, not a tag object.
	gta(w, &["tag", "v-light"]);
	assert_eq!(git(w, &["cat-file", "-t", "v-light"]).trim(), "commit");
	assert_eq!(git(w, &["rev-parse", "v-light"]).trim(), head);

	std::fs::remove_dir_all(&work).ok();
}

fn skip() -> bool {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return true;
	}
	if !have_ssh_keygen() {
		eprintln!("skipping: ssh-keygen not available");
		return true;
	}
	false
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

fn git(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

/// `git tag -v <name>`, returning its stderr (where git prints the verification status), asserting the
/// command succeeded — a good signature exits 0.
fn git_verify(dir: &str, name: &str) -> String {
	let out = Command::new("git")
		.args(["-C", dir, "tag", "-v", name])
		.output()
		.expect("run git tag -v");
	assert!(
		out.status.success(),
		"git tag -v {name} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stderr).expect("git stderr utf8")
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

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-tag");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}

fn have_ssh_keygen() -> bool {
	// `-?` prints usage and exits without touching the filesystem; we only care that it spawned.
	Command::new("ssh-keygen").arg("-?").output().is_ok()
}
