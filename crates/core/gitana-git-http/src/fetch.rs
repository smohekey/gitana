//! The protocol-v2 `fetch` command: want/have negotiation and the side-band pack
//! stream, invoked by `POST /git-upload-pack`.
//!
//! Stateless flow (gitprotocol-v2(5)):
//! - The client sends `want`s, any `have`s it knows, and `done` when it is finished
//!   negotiating. A clone (no local objects) sends `done` immediately.
//! - With `done`, the server replies with the `packfile` section directly.
//! - Without `done`, the server replies with an `acknowledgments` section: `ACK` for
//!   each common object, then — if it found a base to cut at (`ready`) — the
//!   `packfile` section follows in the same response; otherwise `NAK` and the client
//!   negotiates another round.

use std::collections::HashSet;

use gitana_file_store::FileStore;
use gitana_object::{
	HashAlgorithm, ObjectId, PktLine, parse_pkt, write_delim, write_flush, write_pkt,
};
use gitana_repository::Repository;

use crate::GitHttpError;
use crate::deepen::Deepen;
use crate::pack::{build_pack, build_pack_shallow, build_pack_thin};
use crate::shallow::{compute_shallow, reachable_commits, reachable_tag_wants};
use crate::sideband::write_sideband_pack;

/// Parsed `fetch` arguments.
struct FetchArgs<H: HashAlgorithm> {
	/// The objects the client wants.
	wants: Vec<ObjectId<H>>,
	/// The objects the client claims to already have.
	haves: Vec<ObjectId<H>>,
	/// The commits at the client's current shallow boundary (`shallow <oid>` args).
	client_shallow: Vec<ObjectId<H>>,
	/// The client's history-deepening directive (`deepen*` args).
	deepen: Deepen,
	/// The client has finished negotiating.
	done: bool,
	/// The client requested `thin-pack` (deltas may reference bases it already has).
	thin: bool,
	/// The client requested `include-tag` (append reachable annotated tags to the pack).
	include_tag: bool,
}

/// Handle a v2 `fetch` request body, returning the negotiation + packfile response.
pub async fn fetch<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	request: &[u8],
) -> Result<Vec<u8>, GitHttpError> {
	let args = parse_fetch(request)?;
	if args.wants.is_empty() {
		return Err(GitHttpError::MalformedRequest(
			"fetch with no wants".to_owned(),
		));
	}

	let mut out = Vec::new();

	if args.done {
		// Negotiation already concluded: send the pack straight away.
		return finish_with_pack(out, repo, &args).await;
	}

	// Negotiation round: acknowledge the haves we actually have.
	let common = common_haves(repo, &args.haves).await?;
	write_pkt(&mut out, b"acknowledgments\n")?;
	if common.is_empty() {
		// No shared history yet — the client will send another round (with `done`).
		write_pkt(&mut out, b"NAK\n")?;
		write_flush(&mut out);
		return Ok(out);
	}
	for oid in &common {
		write_pkt(&mut out, format!("ACK {oid}\n").as_bytes())?;
	}
	// We have a cut point, so we can build the pack now.
	write_pkt(&mut out, b"ready\n")?;
	write_delim(&mut out);
	finish_with_pack(out, repo, &args).await
}

/// Append the `packfile` section (side-band pack) and close the response, preceded by a `shallow-info`
/// section when the client requested a shallow history.
async fn finish_with_pack<H: HashAlgorithm>(
	mut out: Vec<u8>,
	repo: &Repository<impl FileStore, H>,
	args: &FetchArgs<H>,
) -> Result<Vec<u8>, GitHttpError> {
	// The client's own shallow commits always bound the have-walk (it lacks their parents), whether or
	// not this request deepens — otherwise a plain fetch from a shallow clone could subtract ancestors
	// the client does not actually have.
	let have_boundary: HashSet<ObjectId<H>> = args.client_shallow.iter().copied().collect();

	// Only an actual `deepen*` directive recomputes the boundary and emits a `shallow-info` section; a
	// normal fetch from a shallow client keeps its boundary and gets a plain (have-bounded) pack.
	let mut wants = args.wants.clone();
	let mut boundary = HashSet::new();
	let mut shallow_included: Option<HashSet<ObjectId<H>>> = None;
	if !args.deepen.is_empty() {
		let plan = compute_shallow(repo, &args.wants, &args.deepen, &args.client_shallow).await?;
		if !plan.shallow.is_empty() || !plan.unshallow.is_empty() {
			write_pkt(&mut out, b"shallow-info\n")?;
			for oid in &plan.shallow {
				write_pkt(&mut out, format!("shallow {oid}\n").as_bytes())?;
			}
			for oid in &plan.unshallow {
				write_pkt(&mut out, format!("unshallow {oid}\n").as_bytes())?;
			}
			write_delim(&mut out); // end shallow-info, before the packfile section
		}
		// Seed the walk with the newly-exposed ancestors (deepen / `--unshallow`), which the client's
		// tips do not reach on their own.
		wants.extend(plan.send_roots.iter().copied());
		boundary = plan.boundary;
		shallow_included = Some(plan.included);
	}
	// `include-tag`: append the annotated tags reachable within what this request sends, so a
	// single-branch clone/fetch (shallow or not) still receives tags pointing into the fetched history.
	if args.include_tag {
		let included = match shallow_included {
			Some(included) => included,
			None => reachable_commits(repo, &args.wants, &args.haves).await?,
		};
		wants.extend(reachable_tag_wants(repo, &included).await?);
	}

	write_pkt(&mut out, b"packfile\n")?;
	let shallow_context = !boundary.is_empty() || !have_boundary.is_empty();
	let pack = if shallow_context {
		build_pack_shallow(repo, &wants, &args.haves, &boundary, &have_boundary).await?
	} else if args.thin {
		build_pack_thin(repo, &wants, &args.haves).await?
	} else {
		build_pack(repo, &wants, &args.haves).await?
	};
	write_sideband_pack(&mut out, &pack)?;
	write_flush(&mut out);
	Ok(out)
}

/// The subset of `haves` the server actually has (its negotiation cut points).
async fn common_haves<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	haves: &[ObjectId<H>],
) -> Result<Vec<ObjectId<H>>, GitHttpError> {
	let store = repo.objects();
	let mut common = Vec::new();
	for &have in haves {
		if store.exists_object(&have).await? {
			common.push(have);
		}
	}
	Ok(common)
}

/// Parse the `fetch` body: the `command=fetch` line and capabilities, a delimiter,
/// then `want <oid>` / `have <oid>` / `done` arguments.
fn parse_fetch<H: HashAlgorithm>(request: &[u8]) -> Result<FetchArgs<H>, GitHttpError> {
	let mut args = FetchArgs {
		wants: Vec::new(),
		haves: Vec::new(),
		client_shallow: Vec::new(),
		deepen: Deepen::default(),
		done: false,
		thin: false,
		include_tag: false,
	};
	let mut saw_command = false;

	let mut cursor = 0;
	while cursor < request.len() {
		let (line, consumed) = parse_pkt(&request[cursor..])?;
		cursor += consumed;
		let PktLine::Data(data) = line else {
			if line == PktLine::Flush {
				break;
			}
			continue;
		};
		let text = std::str::from_utf8(data)
			.map_err(|_| GitHttpError::MalformedRequest("non-utf8 pkt-line".to_owned()))?
			.trim_end_matches('\n');

		if text == "command=fetch" {
			saw_command = true;
		} else if text == "done" {
			args.done = true;
		} else if text == "thin-pack" {
			args.thin = true;
		} else if text == "include-tag" {
			args.include_tag = true;
		} else if text == "deepen-relative" {
			args.deepen.relative = true;
		} else if let Some(oid) = text.strip_prefix("want ") {
			args.wants.push(parse_oid(oid)?);
		} else if let Some(oid) = text.strip_prefix("have ") {
			args.haves.push(parse_oid(oid)?);
		} else if let Some(oid) = text.strip_prefix("shallow ") {
			args.client_shallow.push(parse_oid(oid)?);
		} else if let Some(rest) = text.strip_prefix("deepen ") {
			args.deepen.depth =
				Some(rest.trim().parse().map_err(|_| {
					GitHttpError::MalformedRequest(format!("invalid deepen depth: {rest:?}"))
				})?);
		} else if let Some(rest) = text.strip_prefix("deepen-since ") {
			args.deepen.since = Some(rest.trim().parse().map_err(|_| {
				GitHttpError::MalformedRequest(format!("invalid deepen-since time: {rest:?}"))
			})?);
		} else if let Some(rest) = text.strip_prefix("deepen-not ") {
			args.deepen.not.push(rest.trim().to_owned());
		}
	}

	if !saw_command {
		return Err(GitHttpError::MalformedRequest(
			"not a fetch command".to_owned(),
		));
	}
	Ok(args)
}

/// Parse a hex object id (trimming any trailing token, e.g. v0 capabilities).
fn parse_oid<H: HashAlgorithm>(text: &str) -> Result<ObjectId<H>, GitHttpError> {
	let hex = text.split_whitespace().next().unwrap_or("");
	ObjectId::from_hex(hex).map_err(GitHttpError::from)
}
