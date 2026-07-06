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
	HashAlgorithm, ObjectId, ObjectKind, PktLine, decode_pack_with_bases, parse_pkt,
	ref_delta_base_ids, referenced_ids, write_flush, write_pkt,
};
use gitana_repository::{Repository, RepositoryError};
use gitana_trust::AuditEvent;

use crate::push_cert::{self, PushCert};
use crate::{GitHttpError, RefUpdate, TrustContext, TrustVerdict, verify_push};

/// One ref-update command from the client.
struct Command<H: HashAlgorithm> {
	/// Expected current value (`None` when creating a new ref).
	old: Option<ObjectId<H>>,
	/// New value (`None` when deleting the ref).
	new: Option<ObjectId<H>>,
	/// The ref name (`refs/heads/main`, …).
	name: String,
}

impl<H: HashAlgorithm> Command<H> {
	/// The public [`RefUpdate`] view trust enforcement consumes. (`Command` mirrors `RefUpdate`; the
	/// duplication is a known smell noted for a later cleanup.)
	fn to_update(&self) -> RefUpdate<H> {
		RefUpdate {
			old: self.old,
			new: self.new,
			name: self.name.clone(),
		}
	}
}

/// The host inputs a receive-pack needs beyond the request body: whether destructive updates are
/// permitted, and the trust context and clock the pre-receive trust check runs against.
pub struct ReceiveOptions<'a> {
	/// Permits the destructive updates git withholds by default: non-fast-forward ref updates and
	/// ref deletions. The host grants it only to a sufficiently privileged capability (the
	/// trust/security model gates it on `admin`).
	pub force: bool,
	/// The server identity and nonce secret trust enforcement verifies push certificates against.
	/// A host that has not configured trust passes [`TrustContext::none`]; a trust-configured
	/// repository must supply real values or protected pushes fail closed.
	pub trust: &'a TrustContext,
	/// The current unix time, for push-certificate nonce freshness. The host supplies it so this
	/// crate stays clock-free.
	pub now: u64,
}

/// The result of a receive-pack: the `report-status` bytes and the refs that were
/// successfully updated so the host can react to accepted ref changes.
pub struct ReceiveOutcome<H: HashAlgorithm> {
	/// The `report-status` response body.
	pub report: Vec<u8>,
	/// `(ref name, new oid)` for each accepted (non-delete) update.
	pub updated: Vec<(String, ObjectId<H>)>,
	/// The push certificate, if the client signed the push (`git push --signed`). The
	/// wire codec only surfaces it; policy belongs to the embedding host.
	pub push_cert: Option<PushCert>,
	/// The trust-policy audit trail for this push (`docs/hlds/secure-git-trust-signing.md`, step 7):
	/// whether it was accepted (with any `warn`-mode warnings), rejected outright, or had specific
	/// refs rejected. The wire report does not carry these; a host records them for audit. Empty when
	/// the push failed before trust ran (a bad pack or a connectivity gap).
	pub audit: Vec<AuditEvent>,
}

/// Handle a receive-pack request body, returning the report and the accepted updates.
///
/// The pipeline unpacks, connectivity-checks, runs the pre-receive trust check
/// ([`verify_push`]), and only then writes objects and moves refs — a rejection at any stage
/// leaves the repository untouched. See [`ReceiveOptions`] for the `force` grant and the trust
/// inputs.
pub async fn receive_pack<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	request: &[u8],
	options: ReceiveOptions<'_>,
) -> Result<ReceiveOutcome<H>, GitHttpError> {
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
				audit: Vec::new(),
			});
		}
	};
	let new_tips: Vec<ObjectId<H>> = commands.iter().filter_map(|command| command.new).collect();
	if let Err(reason) = check_connectivity(repo, &objects, &new_tips).await? {
		return Ok(ReceiveOutcome {
			report: report_unpack_failure(out, &commands, &reason),
			updated: Vec::new(),
			push_cert,
			audit: Vec::new(),
		});
	}

	// Pre-receive trust enforcement: verify the candidate trust root, push certificate, and newly
	// introduced signed objects against the repository's policy — reading the pushed-but-unwritten
	// objects through a quarantine overlay — before anything is committed.
	let updates: Vec<RefUpdate<H>> = commands.iter().map(Command::to_update).collect();
	let verdict = verify_push(
		repo,
		options.trust,
		&updates,
		&objects,
		push_cert.as_ref(),
		options.now,
	)
	.await?;
	// Split the verdict into the refs trust rejects (denied before they reach `apply_command`), the
	// `warn`-mode warnings, and the audit events for the rejections. The acceptance event is built
	// *after* the apply loop from the refs that actually landed — trust clearing a ref does not mean
	// it moves (a non-fast-forward, denied deletion, or stale old id still `ng`s it on the wire).
	let (rejected, warnings, mut audit): (HashMap<String, String>, Vec<String>, Vec<AuditEvent>) =
		match verdict {
			TrustVerdict::Accept { warnings } => (HashMap::new(), warnings, Vec::new()),
			// A whole-push rejection (bad certificate, unverifiable root) `ng`s every ref and writes
			// nothing — the repository is left exactly as it was. The verdict may also carry per-ref
			// failures (a `require` push both missing a certificate and introducing an unsigned
			// object): the wire report is a blanket rejection, but the audit trail keeps every reason.
			TrustVerdict::Reject {
				global: Some(reason),
				refs,
			} => {
				let names: Vec<String> = commands.iter().map(|c| c.name.clone()).collect();
				let report = rejection_report(&names, &reason);
				let mut audit = vec![AuditEvent::PushRejected { reason }];
				audit.extend(
					refs
						.into_iter()
						.map(|(name, reason)| AuditEvent::RefRejected { name, reason }),
				);
				return Ok(ReceiveOutcome {
					report,
					updated: Vec::new(),
					push_cert,
					audit,
				});
			}
			// Per-ref rejections `ng` only the named refs; the rest of the push still applies.
			TrustVerdict::Reject { global: None, refs } => {
				let audit = refs
					.iter()
					.map(|(name, reason)| AuditEvent::RefRejected {
						name: name.clone(),
						reason: reason.clone(),
					})
					.collect();
				(refs.into_iter().collect(), Vec::new(), audit)
			}
		};

	// Migrate only the objects reachable from accepted updates. A trust-rejected ref must not leave
	// its (unsigned/untrusted) objects behind in the store — objects reachable from an accepted ref
	// are the ones verify_push cleared (or the repo has no policy). The rest stay in quarantine and
	// are discarded with this request.
	let accepted_tips: Vec<ObjectId<H>> = commands
		.iter()
		.filter(|command| !rejected.contains_key(&command.name))
		.filter_map(|command| command.new)
		.collect();
	let migrate = reachable_pushed(&objects, &accepted_tips)?;
	for (id, (kind, data)) in &objects {
		if migrate.contains(id) {
			repo.objects().write_object(*kind, data).await?;
		}
	}
	write_pkt(&mut out, b"unpack ok\n")?;
	let mut updated = Vec::new();
	let mut applied = Vec::new();
	for command in &commands {
		if let Some(reason) = rejected.get(&command.name) {
			write_pkt(
				&mut out,
				format!("ng {} {reason}\n", command.name).as_bytes(),
			)?;
			continue;
		}
		match apply_command(repo, command, options.force).await {
			Ok(()) => {
				write_pkt(&mut out, format!("ok {}\n", command.name).as_bytes())?;
				applied.push(command.name.clone());
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
	// Record the acceptance for the refs that actually moved (and any `warn`-mode warnings). If
	// nothing landed and there is nothing to warn about, there is no acceptance to audit.
	if !applied.is_empty() || !warnings.is_empty() {
		audit.push(AuditEvent::PushAccepted {
			refs: applied,
			warnings,
		});
	}
	Ok(ReceiveOutcome {
		report: out,
		updated,
		push_cert,
		audit,
	})
}

/// The ref names a request would update (signed `push-cert` or plain command list),
/// parsed without applying anything — for policy decisions and rejection reports.
pub fn command_ref_names<H: HashAlgorithm>(request: &[u8]) -> Vec<String> {
	match parse_receive::<H>(request) {
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

/// Unpack the pushed pack into an `id → (kind, payload)` map, resolving thin-pack
/// bases from the store. An empty pack (delete-only push) yields no objects.
async fn unpack<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	pack: &[u8],
) -> Result<HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>, String> {
	if pack.is_empty() {
		return Ok(HashMap::new());
	}
	let base_ids = ref_delta_base_ids::<H>(pack).map_err(|error| error.to_string())?;
	let mut bases: HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)> = HashMap::new();
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
async fn check_connectivity<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	by_id: &HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
	tips: &[ObjectId<H>],
) -> Result<Result<(), String>, GitHttpError> {
	let mut seen: HashSet<ObjectId<H>> = HashSet::new();
	let mut stack: Vec<ObjectId<H>> = tips.to_vec();
	while let Some(id) = stack.pop() {
		if !seen.insert(id) {
			continue;
		}
		if let Some((kind, data)) = by_id.get(&id) {
			stack.extend(referenced_ids::<H>(*kind, data)?);
		} else if !repo.objects().exists_object(&id).await? {
			return Ok(Err(format!("missing object {id}")));
		}
	}
	Ok(Ok(()))
}

/// The subset of pushed `objects` reachable from the accepted `tips` (through commits' trees and
/// parents and tags' targets). Only these migrate to the store; anything a rejected ref introduced
/// stays in quarantine. Ids that resolve outside the pushed set are already stored, so the walk
/// stops at them.
fn reachable_pushed<H: HashAlgorithm>(
	objects: &HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
	tips: &[ObjectId<H>],
) -> Result<HashSet<ObjectId<H>>, GitHttpError> {
	let mut migrate = HashSet::new();
	let mut stack = tips.to_vec();
	while let Some(id) = stack.pop() {
		let Some((kind, data)) = objects.get(&id) else {
			continue;
		};
		if !migrate.insert(id) {
			continue;
		}
		stack.extend(referenced_ids::<H>(*kind, data)?);
	}
	Ok(migrate)
}

/// Apply one ref-update command via compare-and-set. Updates require fast-forward and
/// deletions are refused unless `force` is granted. Returns a `report-status` reason
/// string on rejection.
async fn apply_command<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	command: &Command<H>,
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
async fn is_fast_forward<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	old: ObjectId<H>,
	new: ObjectId<H>,
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
fn report_unpack_failure<H: HashAlgorithm>(
	mut out: Vec<u8>,
	commands: &[Command<H>],
	reason: &str,
) -> Vec<u8> {
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
struct ParsedRequest<'a, H: HashAlgorithm> {
	commands: Vec<Command<H>>,
	push_cert: Option<PushCert>,
	pack: &'a [u8],
}

/// Parse the command section (a plain command list, or a signed `push-cert` block).
fn parse_receive<H: HashAlgorithm>(request: &[u8]) -> Result<ParsedRequest<'_, H>, GitHttpError> {
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
fn parse_command<H: HashAlgorithm>(data: &[u8]) -> Result<Command<H>, GitHttpError> {
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

/// Parse an object id, mapping the all-zero id (sized for `H`) to `None`.
fn parse_oid_opt<H: HashAlgorithm>(text: &str) -> Result<Option<ObjectId<H>>, GitHttpError> {
	if text.len() == H::RAW_LEN * 2 && text.bytes().all(|b| b == b'0') {
		return Ok(None);
	}
	Ok(Some(ObjectId::from_hex(text)?))
}
