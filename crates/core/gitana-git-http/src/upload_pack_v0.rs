//! The protocol-v0 `upload-pack` request (`POST /git-upload-pack` with no `command=`
//! line): `want`/`have`/`done` negotiation, answered with `NAK` and the packfile.
//!
//! v0 is stateless over HTTP: a clone sends its `want`s, a flush, then `done`; an
//! incremental fetch adds `have`s. We acknowledge with a single `NAK` and send a pack
//! of the wants minus the haves — the haves still trim the pack even without per-have
//! `ACK`s, since the client terminated the round with `done`. When the client
//! negotiated `side-band-64k` (as `git clone` does), the pack streams on channel 1.

use std::collections::HashSet;

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId, PktLine, parse_pkt, write_flush, write_pkt};
use gitana_repository::Repository;

use crate::GitHttpError;
use crate::deepen::Deepen;
use crate::pack::{build_pack, build_pack_shallow, build_pack_thin};
use crate::shallow::{compute_shallow, reachable_commits, reachable_tag_wants};
use crate::sideband::write_sideband_pack;

/// Parsed v0 upload-pack arguments.
struct V0Request<H: HashAlgorithm> {
	/// The objects the client wants.
	wants: Vec<ObjectId<H>>,
	/// The objects the client already has.
	haves: Vec<ObjectId<H>>,
	/// The commits at the client's current shallow boundary (`shallow <oid>` lines).
	client_shallow: Vec<ObjectId<H>>,
	/// The client's history-deepening directive (`deepen*` lines).
	deepen: Deepen,
	/// The client sent `done` (a stateless v0 shallow clone first probes for the boundary without it).
	done: bool,
	/// The client negotiated `side-band-64k`.
	sideband: bool,
	/// The client negotiated `thin-pack` (deltas may reference bases it already has).
	thin: bool,
	/// The client negotiated `include-tag` (append reachable annotated tags to the pack).
	include_tag: bool,
}

/// Handle a v0 upload-pack request body, returning `NAK` plus the packfile.
pub async fn upload_pack_v0<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	request: &[u8],
) -> Result<Vec<u8>, GitHttpError> {
	let parsed = parse_v0(request)?;
	if parsed.wants.is_empty() {
		return Err(GitHttpError::MalformedRequest(
			"upload-pack with no wants".to_owned(),
		));
	}

	let mut out = Vec::new();

	// The client's own shallow commits always bound the have-walk (it lacks their parents), whether or
	// not this request deepens.
	let have_boundary: HashSet<ObjectId<H>> = parsed.client_shallow.iter().copied().collect();

	// Only an actual `deepen*` directive recomputes the boundary and emits a shallow-update section; a
	// normal fetch from a shallow client keeps its boundary and gets a plain (have-bounded) pack.
	let mut wants = parsed.wants.clone();
	let mut boundary = HashSet::new();
	let mut shallow_included: Option<HashSet<ObjectId<H>>> = None;
	if !parsed.deepen.is_empty() {
		let plan = compute_shallow(repo, &parsed.wants, &parsed.deepen, &parsed.client_shallow).await?;
		for oid in &plan.shallow {
			write_pkt(&mut out, format!("shallow {oid}\n").as_bytes())?;
		}
		for oid in &plan.unshallow {
			write_pkt(&mut out, format!("unshallow {oid}\n").as_bytes())?;
		}
		write_flush(&mut out); // ends the shallow-update section
		// Stateless v0: the client first probes for the shallow boundary without `done`, and expects
		// only the shallow-update section (no NAK, no pack) — the pack round follows with `done`.
		if !parsed.done {
			return Ok(out);
		}
		// Seed the walk with the newly-exposed ancestors (deepen / `--unshallow`), which the client's
		// tips do not reach on their own.
		wants.extend(plan.send_roots.iter().copied());
		boundary = plan.boundary;
		shallow_included = Some(plan.included);
	}
	// `include-tag`: append the annotated tags reachable within what this request sends, so a
	// single-branch clone/fetch (shallow or not) still receives tags pointing into the fetched history.
	if parsed.include_tag {
		let included = match shallow_included {
			Some(included) => included,
			None => reachable_commits(repo, &parsed.wants, &parsed.haves).await?,
		};
		wants.extend(reachable_tag_wants(repo, &included).await?);
	}

	write_pkt(&mut out, b"NAK\n")?;
	let shallow_context = !boundary.is_empty() || !have_boundary.is_empty();
	let pack = if shallow_context {
		build_pack_shallow(repo, &wants, &parsed.haves, &boundary, &have_boundary).await?
	} else if parsed.thin {
		build_pack_thin(repo, &wants, &parsed.haves).await?
	} else {
		build_pack(repo, &wants, &parsed.haves).await?
	};
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
fn parse_v0<H: HashAlgorithm>(request: &[u8]) -> Result<V0Request<H>, GitHttpError> {
	let mut parsed = V0Request {
		wants: Vec::new(),
		haves: Vec::new(),
		client_shallow: Vec::new(),
		deepen: Deepen::default(),
		done: false,
		sideband: false,
		thin: false,
		include_tag: false,
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
			if parsed.wants.is_empty() {
				parsed.sideband = rest.contains("side-band-64k");
				parsed.thin = rest.contains("thin-pack");
				parsed.include_tag = rest.contains("include-tag");
				// v0 carries `deepen-relative` as a first-want capability (v2 sends it as an argument).
				parsed.deepen.relative = rest.contains("deepen-relative");
			}
			parsed.wants.push(parse_oid(rest)?);
		} else if let Some(rest) = text.strip_prefix("have ") {
			parsed.haves.push(parse_oid(rest)?);
		} else if let Some(rest) = text.strip_prefix("shallow ") {
			parsed.client_shallow.push(parse_oid(rest)?);
		} else if let Some(rest) = text.strip_prefix("deepen ") {
			parsed.deepen.depth = Some(parse_depth(rest)?);
		} else if let Some(rest) = text.strip_prefix("deepen-since ") {
			parsed.deepen.since = Some(parse_since(rest)?);
		} else if let Some(rest) = text.strip_prefix("deepen-not ") {
			parsed.deepen.not.push(rest.trim().to_owned());
		} else if text == "done" {
			parsed.done = true;
		}
		// Capability-only lines need no handling.
	}

	Ok(parsed)
}

/// Parse a `deepen <n>` depth.
fn parse_depth(text: &str) -> Result<u32, GitHttpError> {
	text
		.trim()
		.parse()
		.map_err(|_| GitHttpError::MalformedRequest(format!("invalid deepen depth: {text:?}")))
}

/// Parse a `deepen-since <t>` Unix timestamp.
fn parse_since(text: &str) -> Result<i64, GitHttpError> {
	text
		.trim()
		.parse()
		.map_err(|_| GitHttpError::MalformedRequest(format!("invalid deepen-since time: {text:?}")))
}

/// Parse the leading hex object id from a token, ignoring any trailing capabilities.
fn parse_oid<H: HashAlgorithm>(text: &str) -> Result<ObjectId<H>, GitHttpError> {
	let hex = text.split_whitespace().next().unwrap_or("");
	ObjectId::from_hex(hex).map_err(GitHttpError::from)
}
