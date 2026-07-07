//! Pre-receive trust enforcement (`docs/hlds/secure-git-trust-signing.md`, step 4).
//!
//! [`verify_push`] is the pure verification core that receive-pack runs *after* unpacking and
//! connectivity-checking a push but *before* writing objects or moving refs. It reads the current
//! trust root, then — under the root's policy — verifies the candidate `refs/gitana/trust` update,
//! the push certificate, and every newly introduced signed commit/tag against the folded root.
//!
//! It never touches the store: the pushed objects live in a [`Quarantine`] overlay (pushed objects
//! ∪ the existing store), so verification sees exactly what a successful push would, without
//! committing anything. The verdict carries the policy's decision — accept (with warnings under
//! `warn`) or reject — so the wire layer only has to render it. Trust-ref validity is always
//! enforced (a hard reject regardless of policy): `warn` eases in object-signature enforcement, it
//! must not let the trust root itself be corrupted.
//!
//! [`receive_pack`](crate::receive_pack) runs this after unpacking and the connectivity check but
//! before it writes objects or moves refs, and renders the [`TrustVerdict`] into its report-status.

use std::collections::{HashMap, HashSet};

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, referenced_ids};
use gitana_repository::{Repository, RepositoryError};
use gitana_trust::{
	ObjectSource, Policy, TrustedKey, fold_trust_root, verify_candidate_trust_update, verify_commit,
	verify_pgpsig, verify_sshsig, verify_tag,
};

use crate::push_cert::{PushCert, verify_nonce};
use crate::{GitHttpError, NoReplayCheck, NonceLedger, RefUpdate};

/// The signed trust-state ref. Its updates are verified as a candidate chain and are exempt from the
/// push-certificate and object-signature requirements (the chain is self-authorising).
const TRUST_REF: &str = "refs/gitana/trust";

/// The SSHSIG namespace git uses for signatures, including push certificates.
const GIT_NAMESPACE: &str = "git";

/// The context a host supplies for enforcement: the server's nonce secret and the canonical
/// identity of this repository (for nonce binding and the certificate `pushee` check), plus the
/// nonce freshness window.
pub struct TrustContext {
	/// The HMAC secret this service mints/verifies push nonces with.
	pub nonce_secret: Vec<u8>,
	/// The canonical, service-scoped repository id the nonce is bound to.
	pub repo_id: String,
	/// The canonical repository URL/id a certificate's `pushee` must equal.
	pub pushee: String,
	/// How far a nonce timestamp may be from `now` (seconds).
	pub nonce_slop_secs: u64,
}

impl TrustContext {
	/// A context carrying no server identity or secret. Its empty `nonce_secret` makes
	/// [`verify_cert`] reject outright (a server with no secret cannot verify certificate freshness
	/// or binding), so it is only sound where no protected push certificate is verified: a
	/// repository with no trust root (verification short-circuits to accept) or a test harness that
	/// never enrols trust. On a trust-configured repository it fails protected pushes closed rather
	/// than honouring a forgeable empty-secret nonce.
	pub fn none() -> Self {
		Self {
			nonce_secret: Vec::new(),
			repo_id: String::new(),
			pushee: String::new(),
			nonce_slop_secs: 0,
		}
	}
}

/// The enforcement decision, with the trust root's policy already applied.
#[derive(Debug, PartialEq, Eq)]
pub enum TrustVerdict {
	/// Proceed with the push. `warnings` is non-empty only under `warn` policy (failures recorded,
	/// not enforced).
	Accept {
		/// Human-readable descriptions of failures observed but not enforced.
		warnings: Vec<String>,
	},
	/// Reject the push. `global` fails the whole push (bad certificate, unverifiable current root);
	/// `refs` fails specific refs by name.
	Reject {
		/// A whole-push rejection reason, if any.
		global: Option<String>,
		/// Per-ref `(name, reason)` rejections.
		refs: Vec<(String, String)>,
	},
}

/// A verification failure: what it scopes to, and whether it is *hard* (always rejects, regardless
/// of policy) or *soft* (rejected only under `require`, a warning under `warn`). Trust-ref validity
/// is hard — `warn` eases in object-signature enforcement, it must not let the trust root itself be
/// poisoned.
struct Failure {
	global: bool,
	hard: bool,
	name: String,
	reason: String,
}

impl Failure {
	/// A soft, whole-push failure (a bad push certificate).
	fn global(reason: impl Into<String>) -> Self {
		Self {
			global: true,
			hard: false,
			name: String::new(),
			reason: reason.into(),
		}
	}

	/// A soft, per-ref failure (an unsigned/untrusted object under a protected ref).
	fn refs(name: &str, reason: impl Into<String>) -> Self {
		Self {
			global: false,
			hard: false,
			name: name.to_owned(),
			reason: reason.into(),
		}
	}

	/// A hard, per-ref failure (an invalid trust-ref move): always rejects.
	fn hard_refs(name: &str, reason: impl Into<String>) -> Self {
		Self {
			global: false,
			hard: true,
			name: name.to_owned(),
			reason: reason.into(),
		}
	}

	fn describe(&self) -> String {
		if self.global {
			self.reason.clone()
		} else {
			format!("{}: {}", self.name, self.reason)
		}
	}
}

/// Verify a push against the repository's trust policy, returning the policy's verdict.
///
/// `commands` are the push's ref updates, `objects` the pushed objects (id → kind + raw bytes) that
/// unpacking produced but that are not yet written, and `push_cert` the certificate if the push was
/// signed. `now` is the current unix time (for nonce freshness); the caller supplies it.
///
/// This does no one-time-nonce replay check (v1's stateless nonce accepts replay within the freshness
/// window). A host that wants to reject a replayed nonce calls [`verify_push_with_ledger`] with a
/// [`NonceLedger`] instead.
pub async fn verify_push<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	context: &TrustContext,
	commands: &[RefUpdate<H>],
	objects: &HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
	push_cert: Option<&PushCert>,
	now: u64,
) -> Result<TrustVerdict, GitHttpError> {
	verify_push_with_ledger(
		repo,
		context,
		commands,
		objects,
		push_cert,
		now,
		&NoReplayCheck,
	)
	.await
}

/// [`verify_push`] plus a one-time-nonce replay check: after a certificate verifies (signature, fresh
/// nonce, pushee, commands), its nonce is recorded in `nonce_ledger` and a replay — a still-fresh
/// nonce already seen — is treated as a certificate failure (rejected under `require`, warned under
/// `warn`). The ledger is the host's state; the pure verification is otherwise identical.
pub async fn verify_push_with_ledger<F: FileStore, H: HashAlgorithm, L: NonceLedger>(
	repo: &Repository<F, H>,
	context: &TrustContext,
	commands: &[RefUpdate<H>],
	objects: &HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
	push_cert: Option<&PushCert>,
	now: u64,
	nonce_ledger: &L,
) -> Result<TrustVerdict, GitHttpError> {
	let current_tip = repo.refs().resolve(TRUST_REF).await?;

	// Fold the current root (if any). Its policy governs the non-trust protected refs; a trust ref
	// that exists but cannot be folded fails the whole push closed.
	let root = match current_tip {
		Some(tip) => match fold_trust_root(repo, tip).await {
			Ok(root) => Some(root),
			Err(error) => {
				return Ok(TrustVerdict::Reject {
					global: Some(format!("current trust root is unverifiable: {error}")),
					refs: Vec::new(),
				});
			}
		},
		None => None,
	};

	// A bootstrap (no current root) has no policy of its own; enforce its trust-ref validity
	// strictly (as `require`). `off` disables the *soft* checks (certificates, object signatures)
	// but not the *hard* trust-ref chain validation below.
	let policy = root.as_ref().map_or(Policy::Require, |r| r.policy);

	// No root and nothing touches the trust ref → trust is not configured → nothing to enforce.
	let touches_trust = commands.iter().any(|c| c.name == TRUST_REF);
	if root.is_none() && !touches_trust {
		return Ok(TrustVerdict::Accept {
			warnings: Vec::new(),
		});
	}
	// A trust-ref move must stand alone. Otherwise a single push could both install a new root and
	// move protected refs, and those refs would be judged under the *old* policy/keys (skipped under
	// off, warned under warn) yet land under the *new* root — becoming grandfathered history. This
	// also covers bootstrap, where there is no policy or keys to enforce protected refs with.
	if touches_trust
		&& commands
			.iter()
			.any(|c| c.name != TRUST_REF && is_protected(&c.name))
	{
		return Ok(TrustVerdict::Reject {
			global: Some(
				"update the trust root in its own push; protected refs cannot be updated in the same push"
					.to_owned(),
			),
			refs: Vec::new(),
		});
	}

	// Pushed objects are not written yet; verification reads them (and the store) through this overlay.
	let quarantine = Quarantine {
		pushed: objects,
		repo,
	};

	let mut failures = Vec::new();

	// Every move of the trust ref is verified before it lands: a create/update must fold as a valid
	// candidate chain (bootstrap self-signed, or signed by the previous root); a deletion — which
	// would remove the enforcement anchor — is refused outright.
	for command in commands.iter().filter(|c| c.name == TRUST_REF) {
		match command.new {
			Some(new) => {
				if let Err(error) = verify_candidate_trust_update(&quarantine, current_tip, new).await {
					failures.push(Failure::hard_refs(
						&command.name,
						format!("invalid trust update: {error}"),
					));
				}
			}
			None => failures.push(Failure::hard_refs(
				&command.name,
				"removing the trust root is not permitted",
			)),
		}
	}

	// Non-trust protected refs require a push certificate (for updates *and* deletions) and, for
	// updates, trusted signatures on every newly introduced object. These soft checks are skipped
	// under `off`, and at bootstrap (no current root, hence no keys to check against).
	if policy != Policy::Off
		&& let Some(root) = &root
	{
		let protected: Vec<&RefUpdate<H>> = commands
			.iter()
			.filter(|c| c.name != TRUST_REF && is_protected(&c.name))
			.collect();
		if !protected.is_empty() {
			match push_cert {
				None => failures.push(Failure::global(
					"a push certificate is required for protected refs",
				)),
				Some(cert) => {
					if let Err(reason) = verify_cert(cert, &root.keys, context, commands, now) {
						failures.push(Failure::global(reason));
					} else if replayed_nonce(nonce_ledger, cert, context, now).await? {
						failures.push(Failure::global(
							"push certificate nonce has already been used (replay)",
						));
					}
				}
			}
			// Object-signature enforcement grandfathers already-protected history: the baseline is
			// everything reachable from the *current* protected ref tips, not merely what is already
			// in the store (an object first pushed to an unprotected ref must still be signed before a
			// protected ref may point at it).
			if protected.iter().any(|c| c.new.is_some()) {
				let baseline = protected_baseline(&quarantine, repo).await?;
				for command in &protected {
					if let Some(new) = command.new
						&& let Some(reason) =
							verify_protected_tip(&quarantine, &command.name, new, &baseline, &root.keys).await?
					{
						failures.push(Failure::refs(&command.name, reason));
					}
				}
			}
		}
	}

	Ok(apply_policy(policy, failures))
}

/// Whether `name` is a ref the trust policy protects: branches, tags, and the reserved
/// `refs/gitana/*` namespace (the trust ref itself is handled separately).
fn is_protected(name: &str) -> bool {
	name.starts_with("refs/heads/")
		|| name.starts_with("refs/tags/")
		|| name.starts_with("refs/gitana/")
}

/// Resolve the collected failures into a [`TrustVerdict`] under `policy`. Hard failures (invalid
/// trust-ref moves) always reject; soft failures (bad cert, unsigned objects) reject only under
/// `require` and are otherwise recorded as warnings.
fn apply_policy(policy: Policy, failures: Vec<Failure>) -> TrustVerdict {
	let (hard, soft): (Vec<Failure>, Vec<Failure>) = failures.into_iter().partition(|f| f.hard);

	let mut rejected = hard;
	let mut warnings = Vec::new();
	if policy == Policy::Require {
		rejected.extend(soft);
	} else {
		// `off` returned before reaching here; this is `warn`. Soft failures are recorded only.
		warnings = soft.iter().map(Failure::describe).collect();
	}

	if rejected.is_empty() {
		return TrustVerdict::Accept { warnings };
	}
	let mut global = None;
	let mut refs = Vec::new();
	for failure in rejected {
		if failure.global {
			global.get_or_insert(failure.reason);
		} else {
			refs.push((failure.name, failure.reason));
		}
	}
	TrustVerdict::Reject { global, refs }
}

/// Verify a push certificate against the trusted `keys`: its signature (SSHSIG or OpenPGP, matching
/// the `gpg.format` the client signed with) over the certificate payload, a fresh repo-bound nonce,
/// the expected `pushee`, and that its signed commands exactly match the push. Returns the rejection
/// reason on the first failure.
fn verify_cert<H: HashAlgorithm>(
	cert: &PushCert,
	keys: &[TrustedKey],
	context: &TrustContext,
	commands: &[RefUpdate<H>],
	now: u64,
) -> Result<(), String> {
	// A server with no configured nonce secret cannot bind or freshness-check a certificate; its
	// nonce HMAC would be computed with a publicly-known empty key. Fail closed rather than accept a
	// forgeable nonce (this is where a `TrustContext::none()` lands on a trust-configured repo).
	if context.nonce_secret.is_empty() {
		return Err("server is not configured to verify push certificates".to_owned());
	}
	// Dispatch on the certificate signature's armor, as commit/tag verification does: git signs a push
	// certificate with the configured `gpg.format`, so an OpenPGP-signed cert (from a `gpg.format=openpgp`
	// client) must verify via the OpenPGP path, an SSHSIG cert in git's `git` namespace.
	let signature = cert.signature.as_bytes();
	let verified = if signature.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
		verify_pgpsig(&cert.payload(), signature, keys)
	} else {
		verify_sshsig(&cert.payload(), signature, keys, GIT_NAMESPACE)
	};
	verified.map_err(|error| format!("push certificate signature: {error}"))?;
	if !verify_nonce(
		&context.nonce_secret,
		&context.repo_id,
		&cert.nonce,
		now,
		context.nonce_slop_secs,
	) {
		return Err("push certificate nonce is stale or invalid".to_owned());
	}
	if cert.pushee != context.pushee {
		return Err(format!(
			"push certificate pushee {} does not match {}",
			cert.pushee, context.pushee
		));
	}
	if !cert_commands_match(cert, commands) {
		return Err("push certificate commands do not match the pushed updates".to_owned());
	}
	Ok(())
}

/// Record the certificate's (already-verified, fresh) nonce in `ledger` and report whether it was a
/// replay — a still-fresh nonce already seen. The entry only needs to outlive the freshness window;
/// `now + 2 * slop` bounds when the nonce can no longer be fresh, so the ledger may evict past it.
async fn replayed_nonce<L: NonceLedger>(
	ledger: &L,
	cert: &PushCert,
	context: &TrustContext,
	now: u64,
) -> Result<bool, GitHttpError> {
	let expires_at = now.saturating_add(context.nonce_slop_secs.saturating_mul(2));
	ledger
		.check_and_record(&cert.nonce, expires_at)
		.await
		.map_err(|error| GitHttpError::NonceLedger(error.to_string()))
}

/// Whether the certificate's signed commands exactly match the push's commands (as a set of
/// `(old, new, ref)` triples in the certificate's zero-padded hex form).
fn cert_commands_match<H: HashAlgorithm>(cert: &PushCert, commands: &[RefUpdate<H>]) -> bool {
	let zero = "0".repeat(H::RAW_LEN * 2);
	let hex = |id: Option<ObjectId<H>>| id.map_or_else(|| zero.clone(), ObjectId::to_hex);
	let signed: HashSet<(String, String, String)> = cert
		.commands
		.iter()
		.map(|c| (c.old.clone(), c.new.clone(), c.refname.clone()))
		.collect();
	let actual: HashSet<(String, String, String)> = commands
		.iter()
		.map(|c| (hex(c.old), hex(c.new), c.name.clone()))
		.collect();
	signed == actual
}

/// The already-protected history: every object reachable (through commits' parents and tags'
/// targets) from the *current* tips of the protected refs. Objects in this set predate this push's
/// protection and are grandfathered; anything else a protected ref newly reaches must be signed.
///
/// This walks live on every push; a persisted baseline (the HLD's explicit require-time baseline)
/// would make it incremental. Fine while histories are small.
async fn protected_baseline<F: FileStore, H: HashAlgorithm>(
	quarantine: &Quarantine<'_, F, H>,
	repo: &Repository<F, H>,
) -> Result<HashSet<ObjectId<H>>, GitHttpError> {
	let tips = repo
		.refs()
		.list("refs/")
		.await?
		.into_iter()
		.filter(|(name, _)| name != TRUST_REF && is_protected(name))
		.map(|(_, oid)| oid);

	let mut seen = HashSet::new();
	let mut stack: Vec<ObjectId<H>> = tips.collect();
	while let Some(id) = stack.pop() {
		if !seen.insert(id) {
			continue;
		}
		let (kind, data) = quarantine.read_object(&id).await?;
		if matches!(kind, ObjectKind::Commit | ObjectKind::Tag) {
			stack.extend(referenced_ids::<H>(kind, &data)?);
		}
	}
	Ok(seen)
}

/// Verify a protected ref's new tip. A `refs/tags/*` tip must be a signed annotated tag object (a
/// lightweight tag — a bare commit — is not sufficient under `require`). Then every commit/tag the
/// tip newly reaches (outside `baseline`) must carry a trusted signature.
async fn verify_protected_tip<F: FileStore, H: HashAlgorithm>(
	quarantine: &Quarantine<'_, F, H>,
	refname: &str,
	new_tip: ObjectId<H>,
	baseline: &HashSet<ObjectId<H>>,
	keys: &[TrustedKey],
) -> Result<Option<String>, GitHttpError> {
	// The tip's own kind is constrained: a tag ref must be a signed annotated tag object (a
	// lightweight tag — a bare commit — is not sufficient); any other protected ref must be a commit
	// (a tree/blob tip carries no signature and would otherwise slip through the walk).
	let (tip_kind, _) = quarantine.read_object(&new_tip).await?;
	if refname.starts_with("refs/tags/") {
		if tip_kind != ObjectKind::Tag {
			return Ok(Some(format!(
				"protected tag {refname} must point at a signed annotated tag object"
			)));
		}
	} else if tip_kind != ObjectKind::Commit {
		return Ok(Some(format!(
			"protected ref {refname} must point at a signed commit"
		)));
	}

	let mut seen = HashSet::new();
	let mut stack = vec![new_tip];
	while let Some(id) = stack.pop() {
		if !seen.insert(id) || baseline.contains(&id) {
			continue;
		}
		let (kind, data) = quarantine.read_object(&id).await?;
		match kind {
			ObjectKind::Commit => {
				if verify_commit::<H>(&data, keys).is_err() {
					return Ok(Some(format!(
						"commit {id} is unsigned or by an untrusted key"
					)));
				}
				stack.extend(referenced_ids::<H>(kind, &data)?);
			}
			ObjectKind::Tag => {
				if verify_tag(&data, keys).is_err() {
					return Ok(Some(format!("tag {id} is unsigned or by an untrusted key")));
				}
				stack.extend(referenced_ids::<H>(kind, &data)?);
			}
			// Trees/blobs carry no signature; parents and tag targets are reached through the
			// commit/tag, so trees are popped and skipped without descending.
			_ => {}
		}
	}
	Ok(None)
}

/// An [`ObjectSource`] over the pushed-but-unwritten objects, falling back to the store — the
/// quarantine trust folding reads so a candidate trust commit (not yet stored) resolves.
struct Quarantine<'a, F, H: HashAlgorithm> {
	pushed: &'a HashMap<ObjectId<H>, (ObjectKind, Vec<u8>)>,
	repo: &'a Repository<F, H>,
}

impl<F: FileStore, H: HashAlgorithm> ObjectSource<H> for Quarantine<'_, F, H> {
	type Error = RepositoryError;

	async fn read_object(&self, id: &ObjectId<H>) -> Result<(ObjectKind, Vec<u8>), RepositoryError> {
		if let Some(object) = self.pushed.get(id) {
			return Ok(object.clone());
		}
		<Repository<F, H> as ObjectSource<H>>::read_object(self.repo, id).await
	}
}
