//! The `receive-pack` service (push): parse ref-update commands and the packfile,
//! validate, then atomically move refs and report status.
//!
//! Safety ordering (gitprotocol failure modes): the pack is unpacked and every
//! pushed tip is connectivity-checked *before* any object or ref is written — a bad
//! push leaves the repository untouched. Objects are written after validation; refs
//! move via compare-and-set (rejecting non-fast-forward and stale updates). The
//! response is a `report-status`: `unpack ok` / `unpack <err>`, then `ok <ref>` /
//! `ng <ref> <reason>` per command.

use std::collections::{HashMap, HashSet};

use gitana_file_store::FileStore;
use gitana_object::{
	ObjectId, ObjectKind, PktLine, decode_pack_with_bases, parse_pkt, ref_delta_base_ids,
	referenced_ids, write_flush, write_pkt,
};
use gitana_repository::{Repository, RepositoryError};

use crate::GitHttpError;
use crate::push_cert::{self, PushCert};

/// The all-zero object id: a command's "no value" (create has zero old, delete has
/// zero new).
const ZERO_OID: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// One ref-update command from the client.
struct Command {
	/// Expected current value (`None` when creating a new ref).
	old: Option<ObjectId>,
	/// New value (`None` when deleting the ref).
	new: Option<ObjectId>,
	/// The ref name (`refs/heads/main`, …).
	name: String,
}

/// The result of a receive-pack: the `report-status` bytes and the refs that were
/// successfully updated so the host can react to accepted ref changes.
pub struct ReceiveOutcome {
	/// The `report-status` response body.
	pub report: Vec<u8>,
	/// `(ref name, new oid)` for each accepted (non-delete) update.
	pub updated: Vec<(String, ObjectId)>,
	/// The push certificate, if the client signed the push (`git push --signed`). The
	/// wire codec only surfaces it; policy belongs to the embedding host.
	pub push_cert: Option<PushCert>,
}

/// Handle a receive-pack request body, returning the report and the accepted updates.
///
/// `force` permits the destructive updates git withholds by default: non-fast-forward
/// ref updates and ref deletions. The host grants it only to a sufficiently privileged
/// capability (the trust/security model gates it on `admin`).
pub async fn receive_pack<F: FileStore>(
	repo: &Repository<F>,
	request: &[u8],
	force: bool,
) -> Result<ReceiveOutcome, GitHttpError> {
	let ParsedRequest {
		commands,
		push_cert,
		pack,
	} = parse_receive(request)?;
	if commands.is_empty() {
		return Err(GitHttpError::MalformedRequest(
			"no ref-update commands".to_owned(),
		));
	}

	let mut out = Vec::new();

	// Unpack (resolving thin-pack bases from the store), then connectivity-check every
	// pushed tip. Any failure here writes nothing and reports `unpack <err>`.
	let objects = match unpack(repo, pack).await {
		Ok(objects) => objects,
		Err(reason) => {
			return Ok(ReceiveOutcome {
				report: report_unpack_failure(out, &commands, &reason),
				updated: Vec::new(),
				push_cert,
			});
		}
	};
	let by_id: HashMap<ObjectId, &(ObjectKind, Vec<u8>)> =
		objects.iter().map(|(id, obj)| (*id, obj)).collect();
	let new_tips: Vec<ObjectId> = commands.iter().filter_map(|command| command.new).collect();
	if let Err(reason) = check_connectivity(repo, &by_id, &new_tips).await? {
		return Ok(ReceiveOutcome {
			report: report_unpack_failure(out, &commands, &reason),
			updated: Vec::new(),
			push_cert,
		});
	}

	// Validated: persist the objects, then apply each ref update independently.
	for (_, (kind, data)) in &objects {
		repo.objects().write_object(*kind, data).await?;
	}
	write_pkt(&mut out, b"unpack ok\n")?;
	let mut updated = Vec::new();
	for command in &commands {
		match apply_command(repo, command, force).await {
			Ok(()) => {
				write_pkt(&mut out, format!("ok {}\n", command.name).as_bytes())?;
				if let Some(new) = command.new {
					updated.push((command.name.clone(), new));
				}
			}
			Err(reason) => {
				write_pkt(
					&mut out,
					format!("ng {} {reason}\n", command.name).as_bytes(),
				)?;
			}
		}
	}
	write_flush(&mut out);
	Ok(ReceiveOutcome {
		report: out,
		updated,
		push_cert,
	})
}

/// The ref names a request would update (signed `push-cert` or plain command list),
/// parsed without applying anything — for policy decisions and rejection reports.
pub fn command_ref_names(request: &[u8]) -> Vec<String> {
	match parse_receive(request) {
		Ok(parsed) => parsed.commands.into_iter().map(|c| c.name).collect(),
		Err(_) => Vec::new(),
	}
}

/// Build a `report-status` that accepts the unpack but rejects every named ref with
/// `reason` — a policy rejection where the caller applies nothing, leaving refs untouched.
pub fn rejection_report(ref_names: &[String], reason: &str) -> Vec<u8> {
	let mut out = Vec::new();
	let _ = write_pkt(&mut out, b"unpack ok\n");
	for name in ref_names {
		let _ = write_pkt(&mut out, format!("ng {name} {reason}\n").as_bytes());
	}
	write_flush(&mut out);
	out
}

/// Unpack the pushed pack into `(id, (kind, payload))` objects, resolving thin-pack
/// bases from the store. An empty pack (delete-only push) yields no objects.
async fn unpack<F: FileStore>(
	repo: &Repository<F>,
	pack: &[u8],
) -> Result<Vec<(ObjectId, (ObjectKind, Vec<u8>))>, String> {
	if pack.is_empty() {
		return Ok(Vec::new());
	}
	let base_ids = ref_delta_base_ids(pack).map_err(|error| error.to_string())?;
	let mut bases: HashMap<ObjectId, (ObjectKind, Vec<u8>)> = HashMap::new();
	for id in base_ids {
		if let Ok(object) = repo.objects().read_object(&id).await {
			bases.insert(id, object);
		}
	}
	let objects = decode_pack_with_bases(pack, &bases).map_err(|error| error.to_string())?;
	Ok(
		objects
			.into_iter()
			.map(|object| (object.id, (object.kind, object.data)))
			.collect(),
	)
}

/// Every object reachable from a pushed tip must be in the pack or already stored.
/// Existing objects are trusted (the store keeps its own connectivity), so the walk
/// stops at them and only descends into newly pushed objects.
async fn check_connectivity<F: FileStore>(
	repo: &Repository<F>,
	by_id: &HashMap<ObjectId, &(ObjectKind, Vec<u8>)>,
	tips: &[ObjectId],
) -> Result<Result<(), String>, GitHttpError> {
	let mut seen: HashSet<ObjectId> = HashSet::new();
	let mut stack: Vec<ObjectId> = tips.to_vec();
	while let Some(id) = stack.pop() {
		if !seen.insert(id) {
			continue;
		}
		if let Some((kind, data)) = by_id.get(&id) {
			stack.extend(referenced_ids(*kind, data)?);
		} else if !repo.objects().exists_object(&id).await? {
			return Ok(Err(format!("missing object {id}")));
		}
	}
	Ok(Ok(()))
}

/// Apply one ref-update command via compare-and-set. Updates require fast-forward and
/// deletions are refused unless `force` is granted. Returns a `report-status` reason
/// string on rejection.
async fn apply_command<F: FileStore>(
	repo: &Repository<F>,
	command: &Command,
	force: bool,
) -> Result<(), String> {
	let refs = repo.refs();
	match (command.old, command.new) {
		(_, Some(new)) => {
			// Update or create. For an update, require the new tip to descend from the old
			// (fast-forward) unless `force` permits rewriting history.
			if let Some(old) = command.old
				&& !force
				&& !is_fast_forward(repo, old, new)
					.await
					.map_err(|e| e.to_string())?
			{
				return Err("non-fast-forward".to_owned());
			}
			refs
				.update_ref(&command.name, new, command.old)
				.await
				.map_err(reason)
		}
		// Deletion: withheld unless `force` is granted (the `delete-refs` capability).
		(Some(old), None) => {
			if !force {
				return Err("deletion denied".to_owned());
			}
			refs
				.delete_ref(&command.name, Some(old))
				.await
				.map_err(reason)
		}
		(None, None) => Err("no-op command".to_owned()),
	}
}

/// Whether `new` reaches `old` through its history (a fast-forward update).
async fn is_fast_forward<F: FileStore>(
	repo: &Repository<F>,
	old: ObjectId,
	new: ObjectId,
) -> Result<bool, RepositoryError> {
	if old == new {
		return Ok(true);
	}
	Ok(repo.rev_list(&[new]).await?.contains(&old))
}

/// Render a ref-update rejection reason from a repository error.
fn reason(error: RepositoryError) -> String {
	match error {
		RepositoryError::RefMoved { .. } => "fetch first".to_owned(),
		other => other.to_string(),
	}
}

/// Emit a `report-status` for a failed unpack: the error, then `ng` for every command.
fn report_unpack_failure(mut out: Vec<u8>, commands: &[Command], reason: &str) -> Vec<u8> {
	let _ = write_pkt(&mut out, format!("unpack {reason}\n").as_bytes());
	for command in commands {
		let _ = write_pkt(
			&mut out,
			format!("ng {} unpacker error\n", command.name).as_bytes(),
		);
	}
	write_flush(&mut out);
	out
}

/// The parsed command section: the ref-update commands, the push certificate (if the
/// push was signed), and the trailing packfile bytes.
struct ParsedRequest<'a> {
	commands: Vec<Command>,
	push_cert: Option<PushCert>,
	pack: &'a [u8],
}

/// Parse the command section (a plain command list, or a signed `push-cert` block).
fn parse_receive(request: &[u8]) -> Result<ParsedRequest<'_>, GitHttpError> {
	if push_cert::is_push_cert(request) {
		let (cert, pack) = push_cert::parse(request)?;
		let commands = cert
			.commands
			.iter()
			.map(|c| {
				Ok(Command {
					old: parse_oid_opt(&c.old)?,
					new: parse_oid_opt(&c.new)?,
					name: c.refname.clone(),
				})
			})
			.collect::<Result<Vec<_>, GitHttpError>>()?;
		return Ok(ParsedRequest {
			commands,
			push_cert: Some(cert),
			pack,
		});
	}

	let mut commands = Vec::new();
	let mut cursor = 0;
	while cursor < request.len() {
		let (line, consumed) = parse_pkt(&request[cursor..])?;
		cursor += consumed;
		match line {
			PktLine::Flush => break,
			PktLine::Data(data) => commands.push(parse_command(data)?),
			_ => {}
		}
	}
	Ok(ParsedRequest {
		commands,
		push_cert: None,
		pack: &request[cursor..],
	})
}

/// Parse one command line: `<old> <new> <ref>`, with capabilities trailing the first
/// line after a NUL.
fn parse_command(data: &[u8]) -> Result<Command, GitHttpError> {
	// Strip the capability list (after a NUL) carried on the first command line.
	let line = data.split(|&b| b == 0).next().unwrap_or(data);
	let text = std::str::from_utf8(line)
		.map_err(|_| GitHttpError::MalformedRequest("non-utf8 command".to_owned()))?
		.trim_end_matches('\n');
	let mut parts = text.split(' ');
	let old = parts.next().unwrap_or("");
	let new = parts.next().unwrap_or("");
	let name = parts.next().unwrap_or("");
	if name.is_empty() {
		return Err(GitHttpError::MalformedRequest(format!(
			"bad command: {text}"
		)));
	}
	Ok(Command {
		old: parse_oid_opt(old)?,
		new: parse_oid_opt(new)?,
		name: name.to_owned(),
	})
}

/// Parse an object id, mapping the all-zero id to `None`.
fn parse_oid_opt(text: &str) -> Result<Option<ObjectId>, GitHttpError> {
	if text == ZERO_OID {
		return Ok(None);
	}
	Ok(Some(ObjectId::from_hex(text)?))
}
