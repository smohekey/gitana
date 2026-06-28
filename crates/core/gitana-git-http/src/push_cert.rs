//! Git push certificates (`git push --signed`): the signed document a client sends with
//! a push, binding the ref updates, the pusher's identity, and a server-issued nonce so
//! the server can verify *who* pushed *what* and that the push is fresh (not replayed).
//!
//! Wire format (a `push-cert` block replacing the command list in a receive-pack request):
//!
//! ```text
//! PKT "push-cert\0<capabilities>\n"
//! PKT "certificate version 0.1\n"
//! PKT "pusher <ident>\n"
//! PKT "pushee <url>\n"
//! PKT "nonce <nonce>\n"
//! [PKT "push-option <opt>\n"]...
//! PKT "\n"
//! PKT "<old> <new> <ref>\n"...
//! PKT "<armored signature line>\n"...
//! PKT "push-cert-end\n"
//! flush-pkt
//! <packfile>
//! ```
//!
//! The **signed payload** is the certificate body — every line from `certificate version`
//! through the last command (including the blank separator line), exactly as below — and
//! the signature is computed over those bytes. `gitana-trust` verifies the signature; this
//! module owns the wire format and the nonce HMAC. See `docs/hlds/trust.md` (Phase 3).

use gitana_object::{PktLine, parse_pkt, write_flush, write_pkt};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::GitHttpError;

/// The marker that opens a push-cert block (before the NUL-separated capabilities).
const MARKER: &str = "push-cert";
/// The line that closes a push-cert block.
const CERT_END: &str = "push-cert-end";

type HmacSha256 = Hmac<Sha256>;

/// One ref-update command inside a certificate, as raw object-id strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertCommand {
	/// Old value (64 zeros to create).
	pub old: String,
	/// New value (64 zeros to delete).
	pub new: String,
	/// The ref name.
	pub refname: String,
}

/// A parsed push certificate: the signed body fields plus the detached signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCert {
	/// The certificate version (`0.1`).
	pub version: String,
	/// The pusher identity (`Name <email> <timestamp> <tz>`).
	pub pusher: String,
	/// The repository URL the push targets.
	pub pushee: String,
	/// The server-issued nonce echoed back by the client.
	pub nonce: String,
	/// Any push-options carried in the certificate.
	pub push_options: Vec<String>,
	/// The ref updates the certificate attests to.
	pub commands: Vec<CertCommand>,
	/// The armored signature (SSHSIG or OpenPGP) over [`PushCert::payload`].
	pub signature: String,
}

impl PushCert {
	/// The exact bytes the signature is computed over: the certificate body from
	/// `certificate version` through the last command, including the blank separator.
	pub fn payload(&self) -> Vec<u8> {
		let mut out = String::new();
		out.push_str(&format!("certificate version {}\n", self.version));
		out.push_str(&format!("pusher {}\n", self.pusher));
		out.push_str(&format!("pushee {}\n", self.pushee));
		out.push_str(&format!("nonce {}\n", self.nonce));
		for option in &self.push_options {
			out.push_str(&format!("push-option {option}\n"));
		}
		out.push('\n');
		for command in &self.commands {
			out.push_str(&format!(
				"{} {} {}\n",
				command.old, command.new, command.refname
			));
		}
		out.into_bytes()
	}
}

/// Peek the push certificate from a receive-pack request, if the push is signed (a
/// well-formed `push-cert` block). Parses without applying anything — for policy checks.
pub fn peek(request: &[u8]) -> Option<PushCert> {
	if is_push_cert(request) {
		parse(request).ok().map(|(cert, _)| cert)
	} else {
		None
	}
}

/// Whether `request` opens with a push-cert marker (vs. a plain command list).
pub fn is_push_cert(request: &[u8]) -> bool {
	match parse_pkt(request) {
		Ok((PktLine::Data(data), _)) => {
			let head = data.split(|&b| b == 0).next().unwrap_or(data);
			trim_lf(head) == MARKER.as_bytes()
		}
		_ => false,
	}
}

/// Parse a push-cert receive-pack request, returning the certificate and the trailing
/// packfile bytes. Errors if the request is not a well-formed push-cert block.
pub fn parse(request: &[u8]) -> Result<(PushCert, &[u8]), GitHttpError> {
	let mut cursor = 0;
	let mut lines: Vec<String> = Vec::new();
	let mut saw_marker = false;
	let mut saw_end = false;
	while cursor < request.len() {
		let (line, consumed) = parse_pkt(&request[cursor..])?;
		cursor += consumed;
		match line {
			PktLine::Flush => break,
			PktLine::Data(data) => {
				// The opening `push-cert\0<caps>` marker is not part of the signed body.
				if !saw_marker {
					saw_marker = true;
					continue;
				}
				let text = std::str::from_utf8(data)
					.map_err(|_| malformed("non-utf8 push-cert line"))?
					.to_owned();
				if trim_lf(text.as_bytes()) == CERT_END.as_bytes() {
					saw_end = true;
					// A flush follows; the rest is the pack.
					if let Ok((PktLine::Flush, n)) = parse_pkt(&request[cursor..]) {
						cursor += n;
					}
					break;
				}
				lines.push(text);
			}
			_ => {}
		}
	}
	if !saw_end {
		return Err(malformed("push-cert block missing push-cert-end"));
	}
	let cert = parse_body(&lines)?;
	Ok((cert, &request[cursor..]))
}

/// Parse the certificate body lines (everything between the marker and `push-cert-end`).
fn parse_body(lines: &[String]) -> Result<PushCert, GitHttpError> {
	let mut iter = lines.iter().map(|l| l.trim_end_matches('\n'));

	let version = iter
		.next()
		.and_then(|l| l.strip_prefix("certificate version "))
		.ok_or_else(|| malformed("missing certificate version"))?
		.to_owned();

	let mut pusher = None;
	let mut pushee = None;
	let mut nonce = None;
	let mut push_options = Vec::new();
	// Header lines until the blank separator.
	for line in iter.by_ref() {
		if line.is_empty() {
			break;
		} else if let Some(rest) = line.strip_prefix("pusher ") {
			pusher = Some(rest.to_owned());
		} else if let Some(rest) = line.strip_prefix("pushee ") {
			pushee = Some(rest.to_owned());
		} else if let Some(rest) = line.strip_prefix("nonce ") {
			nonce = Some(rest.to_owned());
		} else if let Some(rest) = line.strip_prefix("push-option ") {
			push_options.push(rest.to_owned());
		} else {
			return Err(malformed("unexpected push-cert header line"));
		}
	}

	// Commands until the signature block; then signature lines to the end.
	let mut commands = Vec::new();
	let mut signature = String::new();
	for line in iter.by_ref() {
		if line.starts_with("-----BEGIN ") {
			signature.push_str(line);
			signature.push('\n');
			break;
		}
		commands.push(parse_cert_command(line)?);
	}
	for line in iter {
		signature.push_str(line);
		signature.push('\n');
	}

	Ok(PushCert {
		version,
		pusher: pusher.ok_or_else(|| malformed("missing pusher"))?,
		pushee: pushee.ok_or_else(|| malformed("missing pushee"))?,
		nonce: nonce.ok_or_else(|| malformed("missing nonce"))?,
		push_options,
		commands,
		signature,
	})
}

/// Parse one `<old> <new> <ref>` command line.
fn parse_cert_command(line: &str) -> Result<CertCommand, GitHttpError> {
	let mut parts = line.splitn(3, ' ');
	let old = parts.next().unwrap_or("");
	let new = parts.next().unwrap_or("");
	let refname = parts.next().unwrap_or("");
	if refname.is_empty() {
		return Err(malformed("bad push-cert command"));
	}
	Ok(CertCommand {
		old: old.to_owned(),
		new: new.to_owned(),
		refname: refname.to_owned(),
	})
}

/// Build a push-cert receive-pack request: the cert block, then a flush and the pack.
/// `capabilities` are advertised after the marker's NUL (e.g. `report-status`).
pub fn build(cert: &PushCert, capabilities: &str, pack: &[u8]) -> Vec<u8> {
	let mut out = Vec::new();
	let _ = write_pkt(&mut out, format!("{MARKER}\0{capabilities}\n").as_bytes());
	let _ = write_pkt(
		&mut out,
		format!("certificate version {}\n", cert.version).as_bytes(),
	);
	let _ = write_pkt(&mut out, format!("pusher {}\n", cert.pusher).as_bytes());
	let _ = write_pkt(&mut out, format!("pushee {}\n", cert.pushee).as_bytes());
	let _ = write_pkt(&mut out, format!("nonce {}\n", cert.nonce).as_bytes());
	for option in &cert.push_options {
		let _ = write_pkt(&mut out, format!("push-option {option}\n").as_bytes());
	}
	let _ = write_pkt(&mut out, b"\n");
	for command in &cert.commands {
		let _ = write_pkt(
			&mut out,
			format!("{} {} {}\n", command.old, command.new, command.refname).as_bytes(),
		);
	}
	for line in cert.signature.lines() {
		let _ = write_pkt(&mut out, format!("{line}\n").as_bytes());
	}
	let _ = write_pkt(&mut out, format!("{CERT_END}\n").as_bytes());
	write_flush(&mut out);
	out.extend_from_slice(pack);
	out
}

/// Issue a stateless push nonce: `<timestamp>-<hex(HMAC-SHA256(secret, timestamp))>`.
///
/// Stateless so any server replica can mint and verify without shared state; the HMAC
/// binds the timestamp so a client cannot forge a fresh-looking nonce.
pub fn make_nonce(secret: &[u8], timestamp: u64) -> String {
	let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
	mac.update(timestamp.to_string().as_bytes());
	format!("{timestamp}-{}", hex(&mac.finalize().into_bytes()))
}

/// Verify a nonce minted by [`make_nonce`]: the HMAC must match (constant-time) and the
/// timestamp must be within `slop_secs` of `now` (replay/clock-skew window).
pub fn verify_nonce(secret: &[u8], nonce: &str, now: u64, slop_secs: u64) -> bool {
	let Some((ts_str, tag_hex)) = nonce.split_once('-') else {
		return false;
	};
	let Ok(timestamp) = ts_str.parse::<u64>() else {
		return false;
	};
	let Some(tag) = unhex(tag_hex) else {
		return false;
	};
	let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
	mac.update(ts_str.as_bytes());
	mac.verify_slice(&tag).is_ok() && now.abs_diff(timestamp) <= slop_secs
}

/// Lowercase-hex encode.
fn hex(bytes: &[u8]) -> String {
	let mut out = String::with_capacity(bytes.len() * 2);
	for byte in bytes {
		out.push_str(&format!("{byte:02x}"));
	}
	out
}

/// Decode lowercase/uppercase hex, returning `None` on any non-hex input.
fn unhex(text: &str) -> Option<Vec<u8>> {
	if !text.len().is_multiple_of(2) {
		return None;
	}
	(0..text.len())
		.step_by(2)
		.map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
		.collect()
}

/// Trim a single trailing `\n` from a byte slice.
fn trim_lf(data: &[u8]) -> &[u8] {
	data.strip_suffix(b"\n").unwrap_or(data)
}

/// A malformed-request error.
fn malformed(message: &str) -> GitHttpError {
	GitHttpError::MalformedRequest(message.to_owned())
}
