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

use gitana_file_store::FileStore;
use gitana_object::{
	HashAlgorithm, ObjectId, PktLine, parse_pkt, write_delim, write_flush, write_pkt,
};
use gitana_repository::Repository;

use crate::GitHttpError;
use crate::pack::build_pack;
use crate::sideband::write_sideband_pack;

/// Parsed `fetch` arguments.
struct FetchArgs<H: HashAlgorithm> {
	/// The objects the client wants.
	wants: Vec<ObjectId<H>>,
	/// The objects the client claims to already have.
	haves: Vec<ObjectId<H>>,
	/// The client has finished negotiating.
	done: bool,
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

/// Append the `packfile` section (side-band pack) and close the response.
async fn finish_with_pack<H: HashAlgorithm>(
	mut out: Vec<u8>,
	repo: &Repository<impl FileStore, H>,
	args: &FetchArgs<H>,
) -> Result<Vec<u8>, GitHttpError> {
	write_pkt(&mut out, b"packfile\n")?;
	let pack = build_pack(repo, &args.wants, &args.haves).await?;
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
		done: false,
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
		} else if let Some(oid) = text.strip_prefix("want ") {
			args.wants.push(parse_oid(oid)?);
		} else if let Some(oid) = text.strip_prefix("have ") {
			args.haves.push(parse_oid(oid)?);
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
