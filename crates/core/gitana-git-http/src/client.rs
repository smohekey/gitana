//! Client-side wire helpers for driving a git server (used by `gta`).
//!
//! Pure byte in/out, protocol v0 (the simplest interoperable path: refs arrive in the
//! `GET /info/refs` advertisement, so no separate `ls-refs` round trip). The caller
//! owns the HTTP. Builds upload-pack (fetch) and receive-pack (push) requests, and
//! parses their responses; pairs with the server side in this crate.

use gitana_object::{HashAlgorithm, ObjectId, PktLine, parse_pkt, write_flush, write_pkt};

use crate::GitHttpError;
use crate::advertise::AGENT;

/// The refs (and HEAD target) parsed from a `GET /info/refs` advertisement.
#[derive(Debug, Clone)]
pub struct Advertised<H: HashAlgorithm> {
	/// `(ref name, oid)` pairs, including `HEAD` when present.
	pub refs: Vec<(String, ObjectId<H>)>,
	/// The branch `HEAD` points at, from the `symref=HEAD:<ref>` capability.
	pub head_target: Option<String>,
	/// The push-certificate nonce, from the `push-cert=<nonce>` capability (receive-pack
	/// advertisements only). Present when the server accepts signed pushes.
	pub push_cert_nonce: Option<String>,
}

// Manual `Default` (not derived) so it does not impose `H: Default`.
impl<H: HashAlgorithm> Default for Advertised<H> {
	fn default() -> Self {
		Advertised {
			refs: Vec::new(),
			head_target: None,
			push_cert_nonce: None,
		}
	}
}

impl<H: HashAlgorithm> Advertised<H> {
	/// The oid of a named ref, if advertised.
	pub fn oid_of(&self, name: &str) -> Option<ObjectId<H>> {
		self
			.refs
			.iter()
			.find(|(n, _)| n == name)
			.map(|(_, oid)| *oid)
	}

	/// Branch refs (`refs/heads/*`) and their tips.
	pub fn branches(&self) -> impl Iterator<Item = (&str, ObjectId<H>)> {
		self
			.refs
			.iter()
			.filter(|(name, _)| name.starts_with("refs/heads/"))
			.map(|(name, oid)| (name.as_str(), *oid))
	}
}

/// Peek the advertised `object-format` capability from a `GET /info/refs` advertisement
/// **without** committing to a hash type — the bridge that lets a client choose the
/// negotiated algorithm before it can parse the oid lines. The capability trails the
/// first ref line after a NUL; returns its value (e.g. `"sha1"`/`"sha256"`), or `None`
/// when the server advertised none (treat as git's default, sha1).
pub fn peek_object_format(body: &[u8]) -> Option<String> {
	let mut cursor = 0;
	while cursor < body.len() {
		let (line, consumed) = parse_pkt(&body[cursor..]).ok()?;
		cursor += consumed;
		let PktLine::Data(data) = line else {
			continue;
		};
		// Skip the smart-http service banner; the capabilities trail the first ref line.
		if data.starts_with(b"# service=") {
			continue;
		}
		let nul = data.iter().position(|&b| b == 0)?;
		let caps = std::str::from_utf8(&data[nul + 1..]).ok()?;
		return caps
			.split([' ', '\n'])
			.find_map(|cap| cap.strip_prefix("object-format="))
			.map(str::to_owned);
	}
	None
}

/// Parse a `GET /info/refs` v0 advertisement (the `# service` banner, a flush, then
/// `<oid> <ref>` lines with capabilities trailing the first after a NUL).
pub fn parse_advertisement<H: HashAlgorithm>(body: &[u8]) -> Result<Advertised<H>, GitHttpError> {
	let mut result = Advertised::default();
	let mut cursor = 0;
	while cursor < body.len() {
		let (line, consumed) = parse_pkt(&body[cursor..])?;
		cursor += consumed;
		let PktLine::Data(data) = line else {
			continue;
		};
		// Skip the smart-http service banner.
		if data.starts_with(b"# service=") {
			continue;
		}
		// Capabilities (incl. symref=HEAD:...) trail the first ref line after a NUL.
		let (ref_part, caps) = match data.iter().position(|&b| b == 0) {
			Some(nul) => (&data[..nul], Some(&data[nul + 1..])),
			None => (data, None),
		};
		if let Some(caps) = caps {
			result.head_target = symref_target(caps);
			result.push_cert_nonce = capability_value(caps, "push-cert=");
		}
		let text = std::str::from_utf8(ref_part)
			.map_err(|_| GitHttpError::MalformedRequest("non-utf8 ref line".to_owned()))?
			.trim_end_matches('\n');
		let Some((oid, name)) = text.split_once(' ') else {
			continue;
		};
		// A `^{}` line is git's peel annotation, not a real ref: the empty-repo `capabilities^{}`
		// placeholder, or an annotated tag's peeled target trailing its `refs/tags/<name>` line.
		// Neither is a fetchable/writable ref (fetching a tag object pulls in its peeled target
		// anyway), so drop them — leaving them in would let a `refs/tags/*` refspec, or clone's
		// ref recreation, write a junk `refs/tags/<name>^{}` ref.
		if name.ends_with("^{}") {
			continue;
		}
		result
			.refs
			.push((name.to_owned(), ObjectId::from_hex(oid)?));
	}
	Ok(result)
}

/// Extract the `symref=HEAD:<ref>` target from a capability list.
fn symref_target(caps: &[u8]) -> Option<String> {
	capability_value(caps, "symref=HEAD:")
}

/// The value of a `<prefix><value>` capability token in a capability list.
fn capability_value(caps: &[u8], prefix: &str) -> Option<String> {
	let text = std::str::from_utf8(caps).ok()?;
	text
		.split([' ', '\n'])
		.find_map(|cap| cap.strip_prefix(prefix))
		.map(str::to_owned)
}

/// Build a v0 upload-pack request: `want`s (the first carrying capabilities), a
/// flush, the `have`s, then `done`.
pub fn build_upload_pack_request<H: HashAlgorithm>(
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
) -> Vec<u8> {
	let mut out = Vec::new();
	for (index, want) in wants.iter().enumerate() {
		let line = if index == 0 {
			format!(
				"want {} side-band-64k ofs-delta object-format={} agent={AGENT}\n",
				want.to_hex(),
				H::NAME
			)
		} else {
			format!("want {}\n", want.to_hex())
		};
		let _ = write_pkt(&mut out, line.as_bytes());
	}
	write_flush(&mut out);
	for have in haves {
		let _ = write_pkt(&mut out, format!("have {}\n", have.to_hex()).as_bytes());
	}
	let _ = write_pkt(&mut out, b"done\n");
	out
}

/// Extract the packfile from a v0 upload-pack response: skip the `NAK`/`ACK` lines and
/// reassemble side-band channel 1 (channel 3 is a fatal server error).
pub fn parse_upload_pack_response(body: &[u8]) -> Result<Vec<u8>, GitHttpError> {
	let mut pack = Vec::new();
	let mut cursor = 0;
	while cursor < body.len() {
		let (line, consumed) = parse_pkt(&body[cursor..])?;
		cursor += consumed;
		let PktLine::Data(data) = line else {
			continue;
		};
		match data.first() {
			Some(1) => pack.extend_from_slice(&data[1..]),
			Some(2) => {} // progress
			Some(3) => {
				let message = String::from_utf8_lossy(&data[1..]).into_owned();
				return Err(GitHttpError::MalformedRequest(format!(
					"server error: {message}"
				)));
			}
			// A textual control line (NAK / ACK ...): not pack data.
			_ => {}
		}
	}
	Ok(pack)
}

/// A ref-update to push: the expected remote value, the new value, and the ref name.
#[derive(Clone)]
pub struct RefUpdate<H: HashAlgorithm> {
	/// Expected current remote value (`None` to create).
	pub old: Option<ObjectId<H>>,
	/// New value (`None` to delete).
	pub new: Option<ObjectId<H>>,
	/// The ref name.
	pub name: String,
}

/// Build a receive-pack request: `<old> <new> <ref>` command lines (the first
/// carrying capabilities), a flush, then the raw packfile.
pub fn build_receive_pack_request<H: HashAlgorithm>(
	updates: &[RefUpdate<H>],
	pack: &[u8],
) -> Vec<u8> {
	let mut out = Vec::new();
	for (index, update) in updates.iter().enumerate() {
		let command = format!(
			"{} {} {}",
			oid_or_zero(update.old),
			oid_or_zero(update.new),
			update.name
		);
		let line = if index == 0 {
			format!(
				"{command}\0report-status object-format={} ofs-delta agent={AGENT}\n",
				H::NAME
			)
		} else {
			format!("{command}\n")
		};
		let _ = write_pkt(&mut out, line.as_bytes());
	}
	write_flush(&mut out);
	out.extend_from_slice(pack);
	out
}

/// Parse a `report-status` response, returning `Ok` only if the unpack succeeded and
/// every ref was accepted.
pub fn parse_report_status(body: &[u8]) -> Result<(), GitHttpError> {
	let mut unpack_ok = false;
	let mut failures = Vec::new();
	let mut cursor = 0;
	while cursor < body.len() {
		let (line, consumed) = parse_pkt(&body[cursor..])?;
		cursor += consumed;
		let PktLine::Data(data) = line else {
			continue;
		};
		let text = std::str::from_utf8(data)
			.map_err(|_| GitHttpError::MalformedRequest("non-utf8 report".to_owned()))?
			.trim_end_matches('\n');
		if let Some(rest) = text.strip_prefix("unpack ") {
			if rest == "ok" {
				unpack_ok = true;
			} else {
				return Err(GitHttpError::MalformedRequest(format!(
					"unpack failed: {rest}"
				)));
			}
		} else if let Some(rest) = text.strip_prefix("ng ") {
			failures.push(rest.to_owned());
		}
	}
	if !unpack_ok {
		return Err(GitHttpError::MalformedRequest(
			"no unpack status in report".to_owned(),
		));
	}
	if !failures.is_empty() {
		return Err(GitHttpError::MalformedRequest(format!(
			"ref update rejected: {}",
			failures.join("; ")
		)));
	}
	Ok(())
}

/// Render an oid as hex, or the all-zero id (sized for `H`) for `None`.
fn oid_or_zero<H: HashAlgorithm>(oid: Option<ObjectId<H>>) -> String {
	oid
		.map(|id| id.to_hex())
		.unwrap_or_else(|| "0".repeat(H::RAW_LEN * 2))
}

#[cfg(test)]
mod tests {
	use gitana_object::{ObjectKind, Sha1, Sha256};

	use super::*;

	#[test]
	fn client_requests_advertise_the_object_format() {
		// Both push and fetch command packets must name the hash so the server agrees on
		// the format (and signed/unsigned pushes negotiate the same capability set).
		let sha256 = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"c");
		let push = build_receive_pack_request(
			&[RefUpdate {
				old: None,
				new: Some(sha256),
				name: "refs/heads/main".to_owned(),
			}],
			&[],
		);
		assert!(String::from_utf8_lossy(&push).contains("object-format=sha256"));
		let fetch = build_upload_pack_request(&[sha256], &[]);
		assert!(String::from_utf8_lossy(&fetch).contains("object-format=sha256"));

		let sha1 = ObjectId::<Sha1>::compute(ObjectKind::Commit, b"c");
		let push1 = build_receive_pack_request(
			&[RefUpdate {
				old: None,
				new: Some(sha1),
				name: "refs/heads/main".to_owned(),
			}],
			&[],
		);
		assert!(String::from_utf8_lossy(&push1).contains("object-format=sha1"));
		let fetch1 = build_upload_pack_request(&[sha1], &[]);
		assert!(String::from_utf8_lossy(&fetch1).contains("object-format=sha1"));
	}

	#[test]
	fn parses_advertisement_with_head_symref() {
		let oid = ObjectId::<Sha256>::compute(gitana_object::ObjectKind::Commit, b"c").to_hex();
		let mut body = Vec::new();
		write_pkt(&mut body, b"# service=git-upload-pack\n").unwrap();
		write_flush(&mut body);
		write_pkt(
			&mut body,
			format!("{oid} HEAD\0symref=HEAD:refs/heads/main object-format=sha256\n").as_bytes(),
		)
		.unwrap();
		write_pkt(&mut body, format!("{oid} refs/heads/main\n").as_bytes()).unwrap();
		write_flush(&mut body);

		let adv = parse_advertisement::<Sha256>(&body).expect("parse");
		assert_eq!(adv.head_target.as_deref(), Some("refs/heads/main"));
		assert_eq!(adv.branches().count(), 1);
		assert!(adv.oid_of("refs/heads/main").is_some());
	}

	#[test]
	fn drops_peeled_tag_pseudo_refs() {
		// An annotated tag advertises `refs/tags/v1` (the tag object) then a `refs/tags/v1^{}` peel
		// line naming the commit. The peel line is not a real ref and must not appear in `.refs` —
		// else a `refs/tags/*` refspec, or clone's ref recreation, would write a junk `^{}` ref.
		let tag = ObjectId::<Sha256>::compute(gitana_object::ObjectKind::Tag, b"t").to_hex();
		let commit = ObjectId::<Sha256>::compute(gitana_object::ObjectKind::Commit, b"c").to_hex();
		let mut body = Vec::new();
		write_pkt(
			&mut body,
			format!("{tag} refs/tags/v1\0object-format=sha256\n").as_bytes(),
		)
		.unwrap();
		write_pkt(
			&mut body,
			format!("{commit} refs/tags/v1^{{}}\n").as_bytes(),
		)
		.unwrap();
		write_flush(&mut body);

		let adv = parse_advertisement::<Sha256>(&body).expect("parse");
		assert_eq!(adv.refs.len(), 1);
		assert_eq!(adv.oid_of("refs/tags/v1").unwrap().to_hex(), tag);
		assert!(adv.oid_of("refs/tags/v1^{}").is_none());
	}

	#[test]
	fn report_status_detects_rejection() {
		let mut body = Vec::new();
		write_pkt(&mut body, b"unpack ok\n").unwrap();
		write_pkt(&mut body, b"ng refs/heads/main non-fast-forward\n").unwrap();
		write_flush(&mut body);
		assert!(parse_report_status(&body).is_err());
	}

	#[test]
	fn report_status_accepts_success() {
		let mut body = Vec::new();
		write_pkt(&mut body, b"unpack ok\n").unwrap();
		write_pkt(&mut body, b"ok refs/heads/main\n").unwrap();
		write_flush(&mut body);
		assert!(parse_report_status(&body).is_ok());
	}
}
