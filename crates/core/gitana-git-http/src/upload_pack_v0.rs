//! The protocol-v0 `upload-pack` request (`POST /git-upload-pack` with no `command=`
//! line): `want`/`have`/`done` negotiation, answered with `NAK` and the packfile.
//!
//! v0 is stateless over HTTP: a clone sends its `want`s, a flush, then `done`; an
//! incremental fetch adds `have`s. We acknowledge with a single `NAK` and send a pack
//! of the wants minus the haves — the haves still trim the pack even without per-have
//! `ACK`s, since the client terminated the round with `done`. When the client
//! negotiated `side-band-64k` (as `git clone` does), the pack streams on channel 1.

use gitana_file_store::FileStore;
use gitana_object::{ObjectId, PktLine, parse_pkt, write_flush, write_pkt};
use gitana_repository::Repository;

use crate::GitHttpError;
use crate::pack::build_pack;
use crate::sideband::write_sideband_pack;

/// Parsed v0 upload-pack arguments.
struct V0Request {
	/// The objects the client wants.
	wants: Vec<ObjectId>,
	/// The objects the client already has.
	haves: Vec<ObjectId>,
	/// The client negotiated `side-band-64k`.
	sideband: bool,
}

/// Handle a v0 upload-pack request body, returning `NAK` plus the packfile.
pub async fn upload_pack_v0<F: FileStore>(
	repo: &Repository<F>,
	request: &[u8],
) -> Result<Vec<u8>, GitHttpError> {
	let parsed = parse_v0(request)?;
	if parsed.wants.is_empty() {
		return Err(GitHttpError::MalformedRequest(
			"upload-pack with no wants".to_owned(),
		));
	}

	let mut out = Vec::new();
	write_pkt(&mut out, b"NAK\n")?;
	let pack = build_pack(repo, &parsed.wants, &parsed.haves).await?;
	if parsed.sideband {
		write_sideband_pack(&mut out, &pack)?;
		write_flush(&mut out);
	} else {
		// No side-band: the raw pack follows the NAK and closes the stream.
		out.extend_from_slice(&pack);
	}
	Ok(out)
}

/// Parse the v0 body: `want <oid> [caps]` lines (the first carries capabilities),
/// then `have <oid>` lines and `done`.
fn parse_v0(request: &[u8]) -> Result<V0Request, GitHttpError> {
	let mut parsed = V0Request {
		wants: Vec::new(),
		haves: Vec::new(),
		sideband: false,
	};

	let mut cursor = 0;
	while cursor < request.len() {
		let (line, consumed) = parse_pkt(&request[cursor..])?;
		cursor += consumed;
		let PktLine::Data(data) = line else {
			// flush / delim separate the want and have sections; keep scanning.
			continue;
		};
		let text = std::str::from_utf8(data)
			.map_err(|_| GitHttpError::MalformedRequest("non-utf8 pkt-line".to_owned()))?
			.trim_end_matches('\n');

		if let Some(rest) = text.strip_prefix("want ") {
			// The first want line trails the negotiated capabilities after the oid.
			if parsed.wants.is_empty() && rest.contains("side-band-64k") {
				parsed.sideband = true;
			}
			parsed.wants.push(parse_oid(rest)?);
		} else if let Some(rest) = text.strip_prefix("have ") {
			parsed.haves.push(parse_oid(rest)?);
		}
		// `done` and capability-only lines need no handling.
	}

	Ok(parsed)
}

/// Parse the leading 64-hex object id from a token, ignoring any trailing capabilities.
fn parse_oid(text: &str) -> Result<ObjectId, GitHttpError> {
	let hex = text.split_whitespace().next().unwrap_or("");
	ObjectId::from_hex(hex).map_err(GitHttpError::from)
}
