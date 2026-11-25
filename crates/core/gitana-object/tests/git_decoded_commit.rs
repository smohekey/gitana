//! `decode_commit` reads a commit git wrote in another encoding as `git log` shows it.
#![cfg(unix)]

use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path, process::Command};

use gitana_object::{Sha1, Signature, decode_commit};

/// `git` in `dir`, with no global or system config and no automatic maintenance.
fn git(dir: &Path) -> Command {
	let mut command = Command::new("git");
	command
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
		.arg("-C")
		.arg(dir);
	command
}

fn output(command: &mut Command) -> Vec<u8> {
	let out = command.output().expect("run git");
	assert!(out.status.success(), "{command:?}: {out:?}");
	out.stdout
}

/// For each encoding, a commit git writes in it (its author and committer named in it, its message
/// in it) is decoded to what `git log` shows: the names, the emails and the message, in UTF-8.
#[test]
fn a_commit_git_writes_in_another_encoding_is_decoded_as_git_log_shows_it() {
	// (encoding, a name in it, a message in it)
	let cases: [(&str, &[u8], &[u8]); 7] = [
		(
			"ISO-8859-1",
			b"Andr\xe9",
			b"Caf\xe9 cr\xe8me\n\nBr\xfbl\xe9e.\n",
		),
		("ISO-8859-15", b"\xa4uro", b"Co\xfbt: 5 \xa4\n"),
		(
			"Shift_JIS",
			b"\x83\x65\x83\x58",
			b"\x83\x65\x83\x58\x83\x67\n",
		),
		("KOI8-R", b"\xf0\xc5\xd4\xd2", b"\xf0\xd2\xc9\xd7\xc5\xd4\n"),
		// Names the WHATWG reads as Windows tables: their bytes 80-9F are controls, as iconv reads them.
		("ISO-8859-9", b"\xdeen", b"\x80 \xd0\xdd\xfe\n"),
		("ISO-8859-11", b"\xa1", b"\x80\xa1\xd0\n"),
		// macOS's iconv reads TIS-620's bytes 80-A0 as ISO-8859-11's, as gitana does; glibc's leaves
		// them undefined, so git there doesn't convert such a commit at all (measured with glibc 2.36
		// and 2.39). Only the bytes both read alike are compared.
		("TIS-620", b"\xa1", b"\xa1\xd0\n"),
	];
	for (encoding, name, message) in cases {
		let dir = tempfile::tempdir().expect("temporary repository");
		output(git(dir.path()).args(["init", "-q", "-b", "main"]));
		let message_file = dir.path().join(".git/message");
		std::fs::write(&message_file, message).unwrap();
		let name = OsStr::from_bytes(name);
		output(
			git(dir.path())
				.args(["-c", &format!("i18n.commitEncoding={encoding}")])
				.args(["commit", "-q", "--allow-empty", "-F"])
				.arg(&message_file)
				.env("GIT_AUTHOR_NAME", name)
				.env("GIT_AUTHOR_EMAIL", "a@example.com")
				.env("GIT_COMMITTER_NAME", name)
				.env("GIT_COMMITTER_EMAIL", "c@example.com"),
		);
		let raw = output(git(dir.path()).args(["cat-file", "commit", "HEAD"]));
		assert!(
			raw
				.windows(9 + encoding.len())
				.any(|line| line == format!("encoding {encoding}").as_bytes()),
			"{encoding}: git didn't write the header"
		);
		let shown = String::from_utf8(output(git(dir.path()).args([
			"log",
			"-1",
			"--format=%an%x00%ae%x00%cn%x00%ce%x00%B",
		])))
		.unwrap_or_else(|_| panic!("{encoding}: git log's output isn't UTF-8"));
		let shown: Vec<&str> = shown.split('\0').collect();

		let read = decode_commit::<Sha1>(&raw).unwrap();
		let author = Signature::parse(&read.commit.author).unwrap();
		let committer = Signature::parse(&read.commit.committer).unwrap();
		assert_eq!(author.name, shown[0], "{encoding}");
		assert_eq!(author.email, shown[1], "{encoding}");
		assert_eq!(committer.name, shown[2], "{encoding}");
		assert_eq!(committer.email, shown[3], "{encoding}");
		// git log ends %B's output with a newline of its own.
		assert_eq!(
			read.commit.message,
			shown[4].strip_suffix('\n').unwrap(),
			"{encoding}"
		);
		assert_eq!(read.encoding.as_deref(), Some(encoding));
		assert!(!read.guessed.any(), "{encoding}");
		assert!(read.commit.extra_headers.is_empty(), "{encoding}");
	}
}
