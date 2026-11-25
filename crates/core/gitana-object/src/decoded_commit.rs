//! A commit read as git shows it or carries it into a new commit: its text converted to UTF-8 by its
//! `encoding` header.
//!
//! A commit can declare its text (its author, its committer and its message) in another character
//! set with git's `encoding` header, `ISO-8859-1` say. git converts it to UTF-8 whenever it shows it
//! (`git log`) or carries it into a new commit (`git commit --amend`, cherry-pick, rebase, revert,
//! the "WIP on" message of `git stash`), and the commit it writes has no `encoding` header.
//! [`parse_commit`] is exact instead: it reads the commit's bytes as they are, so it refuses text that
//! isn't UTF-8, and [`encode_commit`](crate::encode_commit) gives back the same bytes, which a
//! signature check needs. [`decode_commit`] is the reading for showing and carrying.

use std::borrow::Cow;

use crate::{Commit, HashAlgorithm, ObjectError, parse_commit};

/// A commit as git shows or carries it: [`decode_commit`]'s result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedCommit<H: HashAlgorithm> {
	/// The commit, its author, committer and message in UTF-8 and its `encoding` header removed, as
	/// the commit git writes from it has none. Its `signature`, if it has one, was made over the
	/// commit's own bytes, not these: check it from the raw bytes
	/// ([`commit_signature_and_payload`](crate::commit_signature_and_payload)).
	pub commit: Commit<H>,
	/// The `encoding` header's value as the commit wrote it (any bytes that aren't UTF-8 replaced),
	/// or `None` without one.
	pub encoding: Option<String>,
	/// Which parts couldn't be decoded by the encoding the commit declared, or as UTF-8 without one,
	/// and were read by git's fallback instead: UTF-8 where they're valid UTF-8, and every other byte
	/// as Latin-1. That's what git writes when it carries such text into a new commit. A part all of
	/// ASCII is never guessed: it reads the same in every encoding git supports.
	pub guessed: CommitParts,
	/// Which of the guessed parts had bytes that aren't UTF-8, read as Latin-1 by the fallback: what
	/// git repairs, and warns of ("commit message did not conform to UTF-8"), when it carries them
	/// into a new commit. A guessed part that isn't repaired was valid UTF-8 under an encoding this
	/// doesn't know, and is read as UTF-8, as git reads it.
	pub repaired: CommitParts,
}

/// A flag for each of a commit's parts that carry text (see [`DecodedCommit::guessed`] and
/// [`DecodedCommit::repaired`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitParts {
	/// The `author` line.
	pub author: bool,
	/// The `committer` line.
	pub committer: bool,
	/// The message.
	pub message: bool,
}

impl CommitParts {
	/// Whether any part is flagged.
	pub fn any(&self) -> bool {
		self.author || self.committer || self.message
	}
}

/// The commit `payload` as git shows it or carries it into a new commit: each of its author line,
/// committer line and message converted to UTF-8 on its own, by the encoding its `encoding` header
/// names (UTF-8 without one), and the header removed. A part that can't be decoded that way (an
/// encoding this doesn't know, or bytes that aren't valid in it) is read as git reads it when it
/// writes: UTF-8 where it's valid, Latin-1 for every other byte; [`DecodedCommit::guessed`] says so,
/// and [`DecodedCommit::repaired`] which of those had bytes read as Latin-1. Unlike git, which gives up on the whole commit if any of it can't be decoded, each part stands
/// on its own, so one bad name doesn't garble a good message.
///
/// Single-byte encodings are read by their own tables, as iconv (and so git) reads them:
/// `ISO-8859-1` (by any of the names iconv knows it by: `latin1`, `latin-1`, `l1`, `cp819`, ...)
/// each byte the character of the same number; `ISO-8859-9`, `ISO-8859-11` and `TIS-620` with
/// bytes 80-9F as control characters, though the WHATWG reads these names as windows-1254 and
/// windows-874; ASCII by its names (`US-ASCII`, `ASCII`, ...) refusing any byte over 7F. Other
/// encodings are read by [`encoding_rs`], by the WHATWG's names and tables (`Shift_JIS`, `EUC-JP`,
/// `GBK`, `Big5`, `EUC-KR`, `KOI8-R`, `ISO-8859-15`, `windows-1252`, ...; and `CP932` as Microsoft's
/// Shift_JIS). For the Chinese, Japanese and Korean encodings those tables aren't quite iconv's:
/// `Shift_JIS`, `EUC-JP` and `ISO-2022-JP` read a few symbols in Microsoft's forms (bytes `81 60`
/// as U+FF5E where iconv reads U+301C, `81 91` as U+FFE0 for U+00A2, ...) and accept Microsoft's
/// extra rows (NEC's, IBM's) that iconv refuses; `GB2312` is read as GBK, `Big5` with Hong Kong's
/// additions, `EUC-KR` as Microsoft's code page 949. UTF-16 and the encodings the WHATWG maps to its
/// "replacement" or "x-user-defined" can't be decoded here: their text is guessed.
///
/// A commit whose structure is wrong is refused as [`parse_commit`] refuses it: the decoding changes
/// only text.
pub fn decode_commit<H: HashAlgorithm>(payload: &[u8]) -> Result<DecodedCommit<H>, ObjectError> {
	let (header, message) = match payload.windows(2).position(|pair| pair == b"\n\n") {
		Some(at) => (&payload[..at], Some(&payload[at + 2..])),
		None => (payload, None),
	};
	let declared = header
		.split(|&byte| byte == b'\n')
		.find_map(|line| line.strip_prefix(b"encoding "));
	let decoder = Decoder::named(declared);
	let mut guessed = CommitParts::default();
	let mut repaired = CommitParts::default();
	let mut out = Vec::with_capacity(payload.len());
	let mut lines = header.split(|&byte| byte == b'\n').peekable();
	let mut first = true;
	while let Some(line) = lines.next() {
		// The `encoding` header goes, with any continuation lines it has.
		if line.starts_with(b"encoding ") {
			while lines.peek().is_some_and(|next| next.starts_with(b" ")) {
				lines.next();
			}
			continue;
		}
		if !first {
			out.push(b'\n');
		}
		first = false;
		if let Some(rest) = line.strip_prefix(b"author ") {
			out.extend_from_slice(b"author ");
			out.extend_from_slice(
				decoder
					.decode(rest, &mut guessed.author, &mut repaired.author)
					.as_bytes(),
			);
		} else if let Some(rest) = line.strip_prefix(b"committer ") {
			out.extend_from_slice(b"committer ");
			out.extend_from_slice(
				decoder
					.decode(rest, &mut guessed.committer, &mut repaired.committer)
					.as_bytes(),
			);
		} else {
			out.extend_from_slice(line);
		}
	}
	if let Some(message) = message {
		out.extend_from_slice(b"\n\n");
		out.extend_from_slice(
			decoder
				.decode(message, &mut guessed.message, &mut repaired.message)
				.as_bytes(),
		);
	}
	Ok(DecodedCommit {
		commit: parse_commit::<H>(&out)?,
		encoding: declared.map(|name| String::from_utf8_lossy(name).into_owned()),
		guessed,
		repaired,
	})
}

/// How a commit's text is decoded, by the encoding it declares.
enum Decoder {
	/// No `encoding` header, or one naming UTF-8: the text must be valid UTF-8.
	Utf8,
	/// ISO-8859-1, each byte the character of the same number, as iconv (and so git) decodes it.
	Latin1,
	/// An ISO single-byte encoding the WHATWG reads as this Windows table (`ISO-8859-9` as
	/// windows-1254, `ISO-8859-11` and `TIS-620` as windows-874): the table's characters, but bytes
	/// 80-9F the C1 control characters, as in the ISO table and as iconv reads them.
	WithControls(&'static encoding_rs::Encoding),
	/// ASCII: only bytes up to 7F.
	Ascii,
	/// An encoding [`encoding_rs`] knows.
	Other(&'static encoding_rs::Encoding),
	/// A name nothing here knows, or an encoding that can't hold a commit's text: guessed.
	Unknown,
}

/// The names iconv knows ISO-8859-1 by, lower-cased. The WHATWG reads most of these as
/// windows-1252, which git doesn't: its bytes 80-9F are C1 control characters in ISO-8859-1.
const LATIN1_NAMES: [&str; 12] = [
	"iso-8859-1",
	"iso8859-1",
	"iso_8859-1",
	"iso88591",
	"iso_8859-1:1987",
	"latin1",
	"latin-1",
	"l1",
	"cp819",
	"ibm819",
	"iso-ir-100",
	"csisolatin1",
];

/// The names iconv knows ASCII by, lower-cased; the WHATWG reads them as windows-1252.
const ASCII_NAMES: [&str; 12] = [
	"ascii",
	"us-ascii",
	"us",
	"ansi_x3.4-1968",
	"ansi_x3.4-1986",
	"iso646-us",
	"iso-ir-6",
	"iso_646.irv:1991",
	"csascii",
	"ibm367",
	"cp367",
	"646",
];

/// The names of the Windows tables the WHATWG reads some ISO names as, by which they are themselves.
const WINDOWS_NAMES: [&str; 8] = [
	"windows-1252",
	"cp1252",
	"x-cp1252",
	"windows-1254",
	"cp1254",
	"x-cp1254",
	"windows-874",
	"dos-874",
];

impl Decoder {
	fn named(declared: Option<&[u8]>) -> Self {
		let Some(declared) = declared else {
			return Decoder::Utf8;
		};
		let Ok(name) = std::str::from_utf8(declared) else {
			return Decoder::Unknown;
		};
		let name = name.trim().to_ascii_lowercase();
		if LATIN1_NAMES.contains(&name.as_str()) {
			return Decoder::Latin1;
		}
		if ASCII_NAMES.contains(&name.as_str()) {
			return Decoder::Ascii;
		}
		// Microsoft's Shift_JIS, by a name the WHATWG doesn't list.
		if name == "cp932" {
			return Decoder::Other(encoding_rs::SHIFT_JIS);
		}
		match encoding_rs::Encoding::for_label(name.as_bytes()) {
			Some(encoding) if encoding == encoding_rs::UTF_8 => Decoder::Utf8,
			// An ISO name the WHATWG reads as a Windows table: that table, with the ISO table's
			// controls. The Windows table by its own name is itself.
			Some(encoding)
				if (encoding == encoding_rs::WINDOWS_1254 || encoding == encoding_rs::WINDOWS_874)
					&& !WINDOWS_NAMES.contains(&name.as_str()) =>
			{
				Decoder::WithControls(encoding)
			}
			Some(encoding)
				if encoding == encoding_rs::WINDOWS_1252 && !WINDOWS_NAMES.contains(&name.as_str()) =>
			{
				Decoder::Latin1
			}
			// A commit's lines can't be UTF-16, and the WHATWG's replacement and x-user-defined
			// encodings don't decode text.
			Some(encoding)
				if encoding == encoding_rs::UTF_16LE
					|| encoding == encoding_rs::UTF_16BE
					|| encoding == encoding_rs::REPLACEMENT
					|| encoding == encoding_rs::X_USER_DEFINED =>
			{
				Decoder::Unknown
			}
			Some(encoding) => Decoder::Other(encoding),
			None => Decoder::Unknown,
		}
	}

	/// `bytes` in UTF-8, setting `guessed` if they had to be read by git's fallback, and `repaired`
	/// if that read any of them as Latin-1.
	fn decode<'a>(&self, bytes: &'a [u8], guessed: &mut bool, repaired: &mut bool) -> Cow<'a, str> {
		let decoded = match self {
			Decoder::Utf8 => std::str::from_utf8(bytes).ok().map(Cow::Borrowed),
			Decoder::Latin1 => Some(Cow::Owned(latin1(bytes))),
			Decoder::WithControls(encoding) => with_controls(encoding, bytes).map(Cow::Owned),
			Decoder::Ascii => bytes
				.is_ascii()
				.then(|| Cow::Borrowed(std::str::from_utf8(bytes).expect("ASCII is UTF-8"))),
			Decoder::Other(encoding) => encoding
				.decode_without_bom_handling_and_without_replacement(bytes)
				.filter(|text| !undefined_in_windows_table(encoding, text)),
			Decoder::Unknown => None,
		};
		match decoded {
			Some(text) => text,
			None => {
				if !bytes.is_ascii() {
					*guessed = true;
				}
				if std::str::from_utf8(bytes).is_err() {
					*repaired = true;
				}
				Cow::Owned(utf8_or_latin1(bytes))
			}
		}
	}
}

/// `bytes` read as ISO-8859-1: each byte the character of the same number.
fn latin1(bytes: &[u8]) -> String {
	bytes.iter().map(|&byte| char::from(byte)).collect()
}

/// Whether `text`, decoded by `encoding`, holds a byte its Windows table leaves undefined: the WHATWG
/// decodes such a byte (`81` in windows-1252, `98` in windows-1251) as the C1 control character of
/// the same number, which no Windows table assigns, where iconv (and so git) refuses it.
fn undefined_in_windows_table(encoding: &'static encoding_rs::Encoding, text: &str) -> bool {
	encoding.name().starts_with("windows-")
		&& text.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c))
}

/// `bytes` read by the single-byte `encoding`, but for bytes 80-9F, which are the C1 control
/// characters; `None` if a byte has no character there.
fn with_controls(encoding: &'static encoding_rs::Encoding, bytes: &[u8]) -> Option<String> {
	let mut text = String::with_capacity(bytes.len());
	for byte in bytes {
		if (0x80..=0x9f).contains(byte) {
			text.push(char::from(*byte));
		} else {
			text.push_str(
				&encoding
					.decode_without_bom_handling_and_without_replacement(std::slice::from_ref(byte))?,
			);
		}
	}
	Some(text)
}

/// `bytes` read as git reads text it can't decode when it writes a commit (`verify_utf8`): UTF-8
/// where it's valid, and every other byte as Latin-1.
fn utf8_or_latin1(bytes: &[u8]) -> String {
	let mut text = String::with_capacity(bytes.len());
	for chunk in bytes.utf8_chunks() {
		text.push_str(chunk.valid());
		text.extend(chunk.invalid().iter().map(|&byte| char::from(byte)));
	}
	text
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::Sha1;

	const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

	/// A commit by `author`, committed by `committer`, declaring `encoding` (if any), saying
	/// `message`.
	fn commit(author: &[u8], committer: &[u8], encoding: Option<&[u8]>, message: &[u8]) -> Vec<u8> {
		let mut payload = format!("tree {TREE}\n").into_bytes();
		payload.extend_from_slice(b"author ");
		payload.extend_from_slice(author);
		payload.extend_from_slice(b" <a@e> 1 +0000\ncommitter ");
		payload.extend_from_slice(committer);
		payload.extend_from_slice(b" <c@e> 2 +0000\n");
		if let Some(encoding) = encoding {
			payload.extend_from_slice(b"encoding ");
			payload.extend_from_slice(encoding);
			payload.push(b'\n');
		}
		payload.extend_from_slice(b"\n");
		payload.extend_from_slice(message);
		payload
	}

	fn decoded(payload: &[u8]) -> DecodedCommit<Sha1> {
		decode_commit::<Sha1>(payload).unwrap()
	}

	/// ISO-8859-1 by any of its names, exactly as git decodes it: bytes 80-9F are the C1 control
	/// characters, not windows-1252's euro sign and quotes. Its header is gone, nothing guessed.
	#[test]
	fn latin1_is_each_byte_its_own_character_by_any_of_its_names() {
		for name in ["ISO-8859-1", "latin1", "latin-1", "ISO8859-1", " L1 "] {
			let read = decoded(&commit(
				b"Andr\xe9",
				b"C",
				Some(name.as_bytes()),
				b"Caf\xe9 \x80\x93\n",
			));
			assert_eq!(read.commit.author, "Andr\u{e9} <a@e> 1 +0000", "{name}");
			assert_eq!(read.commit.message, "Caf\u{e9} \u{80}\u{93}\n", "{name}");
			assert!(read.commit.extra_headers.is_empty(), "{name}");
			assert_eq!(read.encoding.as_deref(), Some(name), "{name}");
			assert!(!read.guessed.any(), "{name}");
		}
		// windows-1252 by its own name is windows-1252.
		let read = decoded(&commit(
			b"A",
			b"C",
			Some(b"windows-1252"),
			b"\x80 \x93q\x94\n",
		));
		assert_eq!(read.commit.message, "\u{20ac} \u{201c}q\u{201d}\n");
	}

	/// Other encodings by encoding_rs: Shift_JIS, KOI8-R, ISO-8859-15.
	#[test]
	fn other_encodings_are_decoded_by_their_names() {
		let read = decoded(&commit(
			b"\x83\x65",
			b"C",
			Some(b"Shift_JIS"),
			b"\x83\x65\x83\x58\x83\x67\n",
		));
		assert_eq!(read.commit.author, "\u{30c6} <a@e> 1 +0000");
		assert_eq!(read.commit.message, "\u{30c6}\u{30b9}\u{30c8}\n");
		let read = decoded(&commit(b"A", b"C", Some(b"KOI8-R"), b"\xf0\xd2\xc9\n"));
		assert_eq!(read.commit.message, "\u{41f}\u{440}\u{438}\n");
		let read = decoded(&commit(b"A", b"C", Some(b"ISO-8859-15"), b"\xa4\n"));
		assert_eq!(read.commit.message, "\u{20ac}\n");
		assert!(!read.guessed.any());
	}

	/// What can't be decoded is read as git reads it when it writes, UTF-8 where valid and Latin-1
	/// byte by byte where not, and said to be guessed: an unknown name, no header with bytes that
	/// aren't UTF-8, bytes invalid in the declared encoding, and an encoding that can't hold text.
	/// ASCII is never guessed.
	#[test]
	fn what_can_t_be_decoded_is_read_as_git_writes_it_and_said_to_be_guessed() {
		for encoding in [
			Some(&b"no-such-thing"[..]),
			None,
			Some(b"UTF-16"),
			Some(b"\xff"),
		] {
			let read = decoded(&commit(b"Andr\xe9", b"C", encoding, b"Caf\xe9 \xc3\xa9\n"));
			assert_eq!(
				read.commit.author, "Andr\u{e9} <a@e> 1 +0000",
				"{encoding:?}"
			);
			assert_eq!(read.commit.message, "Caf\u{e9} \u{e9}\n", "{encoding:?}");
			let both = CommitParts {
				author: true,
				committer: false,
				message: true,
			};
			assert_eq!(read.guessed, both, "{encoding:?}");
			assert_eq!(read.repaired, both, "{encoding:?}");
		}
		// Shift_JIS's E9 needs a second byte; the space after it isn't one.
		let read = decoded(&commit(b"A", b"C", Some(b"Shift_JIS"), b"\xe9 x\n"));
		assert_eq!(read.commit.message, "\u{e9} x\n");
		assert!(read.guessed.message);
		// Plain ASCII under an unknown name reads the same in any encoding: nothing guessed.
		let read = decoded(&commit(b"A", b"C", Some(b"no-such-thing"), b"plain\n"));
		assert!(!read.guessed.any());
		// Valid UTF-8 under an unknown name is read as UTF-8, as git reads it: guessed, but nothing
		// repaired, so nothing git would warn of.
		let read = decoded(&commit(
			b"A",
			b"C",
			Some(b"no-such-thing"),
			b"Caf\xc3\xa9\n",
		));
		assert_eq!(read.commit.message, "Caf\u{e9}\n");
		assert!(read.guessed.message);
		assert!(!read.repaired.any());
		// A byte a Windows table leaves undefined is refused, as iconv refuses it, so the part falls
		// back: 80 isn't then the euro sign.
		for (name, bytes) in [
			("windows-1252", &b"\x80 \x81\n"[..]),
			("windows-1251", b"\x80 \x98\n"),
		] {
			let read = decoded(&commit(b"A", b"C", Some(name.as_bytes()), bytes));
			let expected: String = bytes.iter().map(|&byte| char::from(byte)).collect();
			assert_eq!(read.commit.message, expected, "{name}");
			assert!(read.repaired.message, "{name}");
		}
	}

	/// The single-byte names the WHATWG reads as Windows tables are read as iconv reads them:
	/// ISO-8859-9, ISO-8859-11 and TIS-620 with bytes 80-9F as controls (and the bytes TIS-620
	/// doesn't define refused, so guessed), ASCII refusing what's over 7F; each Windows table by its
	/// own name is itself; and CP932 is Microsoft's Shift_JIS.
	#[test]
	fn single_byte_names_are_their_own_tables_as_iconv_reads_them() {
		let read = decoded(&commit(
			b"A",
			b"C",
			Some(b"ISO-8859-9"),
			b"\x80\x93\xd0\xdd\xfe\n",
		));
		assert_eq!(read.commit.message, "\u{80}\u{93}\u{11e}\u{130}\u{15f}\n");
		assert!(!read.guessed.any());
		let read = decoded(&commit(b"A", b"C", Some(b"windows-1254"), b"\x80\xd0\n"));
		assert_eq!(read.commit.message, "\u{20ac}\u{11e}\n");
		for name in ["ISO-8859-11", "TIS-620"] {
			let read = decoded(&commit(
				b"A",
				b"C",
				Some(name.as_bytes()),
				b"\x80\xa0\xa1\xd0\n",
			));
			assert_eq!(
				read.commit.message, "\u{80}\u{a0}\u{e01}\u{e30}\n",
				"{name}"
			);
			assert!(!read.guessed.any(), "{name}");
			let read = decoded(&commit(b"A", b"C", Some(name.as_bytes()), b"\xdb\n"));
			assert!(read.guessed.message, "{name}");
		}
		for name in ["US-ASCII", "ascii"] {
			let read = decoded(&commit(b"A", b"C", Some(name.as_bytes()), b"Caf\xe9\n"));
			assert_eq!(read.commit.message, "Caf\u{e9}\n", "{name}");
			assert!(read.guessed.message, "{name}");
			let read = decoded(&commit(b"A", b"C", Some(name.as_bytes()), b"plain\n"));
			assert!(!read.guessed.any(), "{name}");
		}
		let read = decoded(&commit(b"A", b"C", Some(b"CP932"), b"\x81\x60\x87\x40\n"));
		assert_eq!(read.commit.message, "\u{ff5e}\u{2460}\n");
		assert!(!read.guessed.any());
	}

	/// Each part stands on its own: a name invalid in Shift_JIS doesn't spoil a valid message.
	#[test]
	fn each_part_is_decoded_on_its_own() {
		let read = decoded(&commit(
			b"Andr\xe9",
			b"\x83\x65",
			Some(b"Shift_JIS"),
			b"\x83\x65\x83\x58\x83\x67\n",
		));
		assert_eq!(read.commit.author, "Andr\u{e9} <a@e> 1 +0000");
		assert_eq!(read.commit.committer, "\u{30c6} <c@e> 2 +0000");
		assert_eq!(read.commit.message, "\u{30c6}\u{30b9}\u{30c8}\n");
		let author = CommitParts {
			author: true,
			committer: false,
			message: false,
		};
		assert_eq!(read.guessed, author);
		assert_eq!(read.repaired, author);
	}

	/// A UTF-8 commit reads as `parse_commit` reads it, other headers kept; a commit whose structure
	/// is wrong is refused as `parse_commit` refuses it; and `parse_commit` still refuses text that
	/// isn't UTF-8.
	#[test]
	fn the_structure_is_parse_commit_s() {
		let mut utf8 = format!("tree {TREE}\nparent {TREE}\n").into_bytes();
		utf8.extend_from_slice(
			"author Andr\u{e9} <a@e> 1 +0000\ncommitter C <c@e> 2 +0000\nmergetag object x\n \
			 type commit\n\nCaf\u{e9}\n"
				.as_bytes(),
		);
		let read = decoded(&utf8);
		assert_eq!(read.commit, parse_commit::<Sha1>(&utf8).unwrap());
		assert_eq!(read.encoding, None);
		assert!(!read.guessed.any());
		assert!(matches!(
			decode_commit::<Sha1>(b"author A <a@e> 1 +0000\n\nm\n"),
			Err(ObjectError::MalformedHeader)
		));
		let latin1 = commit(b"Andr\xe9", b"C", Some(b"ISO-8859-1"), b"x\n");
		assert!(parse_commit::<Sha1>(&latin1).is_err());
	}
}
