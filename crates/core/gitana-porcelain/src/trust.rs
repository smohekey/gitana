//! `trust` — manage a repository's `refs/gitana/trust` chain (see
//! `docs/hlds/secure-git-trust-signing.md`). Each update is a signed commit whose tree carries the
//! canonical trust document; the commit chain *is* the authorization chain. These operations build
//! and sign those commits, and — crucially — re-verify the candidate chain through the same
//! `gitana-trust` core the server enforces with **before** moving the local ref, so a local edit can
//! never install a root the server would reject.

use anyhow::{Context, Result, anyhow, bail};
use gitana_file_store::FileStore;
use gitana_git_http::parse_advertisement;
use gitana_object::{Commit, HashAlgorithm, ObjectId, ObjectKind, encode_commit};
use gitana_remote::{HttpTransport, Origin};
use gitana_repository::{FileMode, Repository, TreeBuildEntry};
use gitana_trust::{
	KeyId, Policy, TRUST_DOCUMENT_PATH, TrustDocument, TrustRoot, TrustedKey, fold_trust_root,
	verify_candidate_trust_update, verify_candidate_trust_update_anchored,
};

use crate::{Identity, Signer};

/// The ref holding a repository's trust state. Its commit chain is the authorization chain.
pub const TRUST_REF: &str = "refs/gitana/trust";

/// Bootstrap trust for a repository: write the first, self-signed trust commit enrolling
/// `signing_pubkey` (the OpenSSH public-key line of the key we sign with) under `policy`, and point
/// `refs/gitana/trust` at it. Returns the new tip.
///
/// Refuses if trust is already initialised (bootstrap happens once). Under [`Policy::Require`] a
/// single enrolled key is unsafe — losing it locks out the repository — so that is refused unless
/// `break_glass` is set (the design's explicit override); enrol a second key first instead. The
/// built commit is re-folded through [`verify_candidate_trust_update`] before the ref moves, so the
/// bootstrap is only adopted if it genuinely self-verifies.
pub async fn trust_init<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	policy: Policy,
	signing_pubkey: &str,
	break_glass: bool,
	identity: &impl Identity,
	signer: &impl Signer,
) -> Result<ObjectId<H>> {
	if repo.refs().resolve(TRUST_REF).await?.is_some() {
		bail!(
			"trust is already initialised ({TRUST_REF} exists); use `add-key`/`remove-key` to change it"
		);
	}
	if policy == Policy::Require && !break_glass {
		bail!(
			"`--policy require` with a single enrolled key is unsafe: losing it locks the repository. \
			 Initialise with `--policy warn`, enrol a second key with `add-key`, or pass \
			 `--break-glass` to override."
		);
	}

	let document = TrustDocument::new(1, policy, vec![signing_pubkey.to_owned()]);
	let tip = write_trust_commit(
		repo,
		&document,
		Vec::new(),
		"gitana trust: bootstrap",
		identity,
		signer,
	)
	.await?;

	// Prove the chain before moving the ref: the bootstrap must be self-signed by a key in its own
	// root. If the signing key is not the one we enrolled, this refuses and the ref stays unset.
	verify_candidate_trust_update(repo, None, tip).await?;

	repo.refs().update_ref(TRUST_REF, tip, None).await?;
	let committer = identity.committer_or_default().await;
	repo
		.refs()
		.append_reflog(TRUST_REF, None, tip, &committer, "trust: bootstrap")
		.await?;
	Ok(tip)
}

/// The current effective trust root: fold the `refs/gitana/trust` chain into its [`TrustRoot`], or
/// `None` when the ref is unset (trust not configured). Folding verifies the whole chain, so a
/// tampered or unverifiable root surfaces here as an error rather than a value.
pub async fn trust_list<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<Option<TrustRoot>> {
	match repo.refs().resolve(TRUST_REF).await? {
		None => Ok(None),
		Some(tip) => Ok(Some(fold_trust_root(repo, tip).await?)),
	}
}

/// The outcome of a [`trust_sync`].
#[derive(Debug)]
pub enum TrustSyncOutcome<H: HashAlgorithm> {
	/// The remote does not publish `refs/gitana/trust`; there was nothing to sync.
	RemoteUnset,
	/// Local trust already contains the remote tip — the refs are equal, or the local chain is ahead
	/// of the remote. Nothing moved.
	UpToDate,
	/// Adopted the remote root: the local `refs/gitana/trust` moved `old` → `new` (a bootstrap when
	/// `old` is `None`).
	Updated {
		old: Option<ObjectId<H>>,
		new: ObjectId<H>,
	},
	/// A bootstrap adoption (local trust was unset) that the caller's `confirm` declined; the remote
	/// tip `new` verified but was not adopted, and the local ref stays unset.
	Declined { new: ObjectId<H> },
}

/// Adopt the remote's `refs/gitana/trust` into the local ref, **forward-only and only if it
/// verifies**. `advertisement` is the already-fetched `git-upload-pack` `GET /info/refs` body (the
/// CLI adapter does the HTTP, keeping this network-free like the other remote composites).
///
/// The remote tip is verified as a candidate update over the *local* tip through the same
/// [`verify_candidate_trust_update`] the server enforces with: the remote chain must fold cleanly
/// (self-signed bootstrap, every link signed by the prior root) and, when local trust already
/// exists, must fast-forward it (the local tip is an ancestor of the remote tip). An invalid or
/// divergent remote root is refused and the local ref never moves. A remote tip the local chain
/// already contains (equal, or local-ahead) is a no-op. Only the remote trust chain's objects are
/// downloaded, not the whole advertisement.
///
/// When local trust is **unset** this is a trust-on-first-use bootstrap: folding proves the chain is
/// internally consistent but cannot prove its bootstrap key is the *right* one, so `confirm` is
/// consulted before adopting. It receives the incoming [`TrustRoot`] and the chain's anchor (the
/// [`KeyId`] that signed the bootstrap — the thing worth pinning; see the crate's `FoldedTrust`):
/// `Ok(true)` adopts, `Ok(false)` yields [`TrustSyncOutcome::Declined`] and leaves the ref unset, and
/// an `Err` propagates. On a fast-forward (local trust already exists and anchors the update)
/// `confirm` is **not** called — the update is already anchored to the trusted local root.
pub async fn trust_sync<F: FileStore, H: HashAlgorithm>(
	transport: &impl HttpTransport,
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	identity: &impl Identity,
	confirm: impl AsyncFnOnce(&TrustRoot, &KeyId) -> Result<bool>,
) -> Result<TrustSyncOutcome<H>> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let Some(remote_tip) = advertised.oid_of(TRUST_REF) else {
		return Ok(TrustSyncOutcome::RemoteUnset);
	};
	let local_tip = repo.refs().resolve(TRUST_REF).await?;
	if local_tip == Some(remote_tip) {
		return Ok(TrustSyncOutcome::UpToDate);
	}

	// Download only the remote trust chain's objects (the tip and its ancestors we lack) so the
	// candidate can be folded and verified locally.
	let haves = gitana_remote::local_haves(repo).await?;
	gitana_remote::fetch_pack(transport, origin, repo, &[remote_tip], &haves).await?;

	// If the local chain already contains the remote tip, we are ahead of the remote: keep the richer
	// local root rather than "rewinding" to it. (`verify_candidate_trust_update` only proves the
	// other direction — local fast-forwards to remote — so this case is handled first.)
	if let Some(local) = local_tip
		&& repo.is_ancestor(remote_tip, local).await?
	{
		return Ok(TrustSyncOutcome::UpToDate);
	}

	// Prove the remote root before adopting it: it must fold cleanly and (when local trust exists)
	// fast-forward the local tip. A divergent chain fails here and the ref stays put. Surface the
	// chain's anchor so a bootstrap adoption can pin it.
	let folded = verify_candidate_trust_update_anchored(repo, local_tip, remote_tip).await?;

	// A first-use bootstrap (no local trust) is faith-based: folding proves internal consistency but
	// not that the anchor is the right key. Defer to `confirm` before adopting. A fast-forward is
	// already anchored to the trusted local root, so it skips the prompt.
	if local_tip.is_none() && !confirm(&folded.root, &folded.anchor).await? {
		return Ok(TrustSyncOutcome::Declined { new: remote_tip });
	}

	repo
		.refs()
		.update_ref(TRUST_REF, remote_tip, local_tip)
		.await?;
	let committer = identity.committer_or_default().await;
	repo
		.refs()
		.append_reflog(TRUST_REF, local_tip, remote_tip, &committer, "trust: sync")
		.await?;
	Ok(TrustSyncOutcome::Updated {
		old: local_tip,
		new: remote_tip,
	})
}

/// Enrol `key_line` (an OpenSSH public-key line) in the trust root, signing the update with `signer`
/// — which must be a key the *current* root already trusts. Refuses a malformed key or one already
/// enrolled (matched by fingerprint, so a re-paste with a different comment is still a duplicate).
/// Extends the chain and moves `refs/gitana/trust` only after the new root re-verifies.
pub async fn trust_add_key<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	key_line: &str,
	identity: &impl Identity,
	signer: &impl Signer,
) -> Result<ObjectId<H>> {
	let tip = current_tip(repo).await?;
	let mut document = read_current_document(repo, tip).await?;

	let new_id = TrustedKey::from_openssh(key_line.trim())
		.context("parsing the public key to add")?
		.id();
	for line in &document.keys {
		let existing = TrustedKey::from_openssh(line)
			.with_context(|| format!("parsing an already-enrolled key ({line})"))?;
		if existing.id() == new_id {
			bail!("key {new_id} is already enrolled");
		}
	}
	document.keys.push(key_line.trim().to_owned());
	trust_update(repo, &document, tip, "add key", identity, signer).await
}

/// Remove the key named by `selector` — a `SHA256:…` fingerprint (as `trust list` prints) or an
/// OpenSSH public-key line — from the trust root, signing the update with `signer`. Refuses when no
/// enrolled key matches, or when it would remove the last key (a root must keep at least one). Under
/// [`Policy::Require`], dropping below two keys is unsafe (the same invariant `init`/`set-policy`
/// hold) and is refused unless `break_glass` is set.
pub async fn trust_remove_key<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	selector: &str,
	break_glass: bool,
	identity: &impl Identity,
	signer: &impl Signer,
) -> Result<ObjectId<H>> {
	let tip = current_tip(repo).await?;
	let mut document = read_current_document(repo, tip).await?;

	let target = selector_fingerprint(selector)?;
	let mut kept = Vec::with_capacity(document.keys.len());
	for line in &document.keys {
		let id = TrustedKey::from_openssh(line)
			.with_context(|| format!("parsing an enrolled key ({line})"))?
			.id();
		if id.as_str() != target {
			kept.push(line.clone());
		}
	}
	if kept.len() == document.keys.len() {
		bail!("no enrolled key matches `{selector}`");
	}
	if kept.is_empty() {
		bail!("cannot remove the last trusted key; a trust root must keep at least one");
	}
	if document.policy == Policy::Require && kept.len() < 2 && !break_glass {
		bail!(
			"removing this key would leave a `require` root with a single key, which is unsafe: \
			 losing it locks the repository. Pass `--break-glass` to override, or lower the policy \
			 with `set-policy` first."
		);
	}
	document.keys = kept;
	trust_update(repo, &document, tip, "remove key", identity, signer).await
}

/// Change the trust policy to `policy`, signing the update with `signer`. Under [`Policy::Require`] a
/// root with fewer than two keys is unsafe — losing the sole key locks the repository — so it is
/// refused unless `break_glass` is set. A no-op change (already `policy`) is refused.
pub async fn trust_set_policy<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	policy: Policy,
	break_glass: bool,
	identity: &impl Identity,
	signer: &impl Signer,
) -> Result<ObjectId<H>> {
	let tip = current_tip(repo).await?;
	let mut document = read_current_document(repo, tip).await?;

	if document.policy == policy {
		bail!("policy is already `{policy}`");
	}
	if policy == Policy::Require && !break_glass && document.keys.len() < 2 {
		bail!(
			"`require` with fewer than two enrolled keys is unsafe: losing the key locks the \
			 repository. Enrol another key with `add-key`, or pass `--break-glass` to override."
		);
	}
	document.policy = policy;
	trust_update(
		repo,
		&document,
		tip,
		&format!("set policy {policy}"),
		identity,
		signer,
	)
	.await
}

/// Write a signed trust commit: store `document` as `trust.json` in a fresh tree, then build a
/// commit over it (with `parents`), sign the exact bytes git signs, and write the signed object.
/// Does not move any ref — the caller verifies the candidate first.
async fn write_trust_commit<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	document: &TrustDocument,
	parents: Vec<ObjectId<H>>,
	subject: &str,
	identity: &impl Identity,
	signer: &impl Signer,
) -> Result<ObjectId<H>> {
	let blob = repo.write_blob(&document.to_json()).await?;
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: TRUST_DOCUMENT_PATH.to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await?;

	let author = identity.author().await?;
	let committer = identity.committer().await?;
	let mut commit = Commit {
		tree,
		parents,
		author,
		committer,
		signature: None,
		extra_headers: Vec::new(),
		message: format!("{subject}\n"),
	};
	// Sign the unsigned encoding (exactly what git signs), then attach the armor and write the final
	// object — matching `verify_commit`, which strips the `gpgsig` header back off to check it.
	let armor = signer.sign(&encode_commit(&commit)).await?;
	commit.signature = Some(armor);
	Ok(
		repo
			.objects()
			.write_object(ObjectKind::Commit, &encode_commit(&commit))
			.await?,
	)
}

/// Extend the trust chain with a new signed commit carrying `document` (parent `old_tip`), re-verify
/// the candidate update through the trust core, then fast-forward `refs/gitana/trust` via CAS.
/// `label` names the operation in the commit subject (`gitana trust: <label>`) and reflog
/// (`trust: <label>`).
async fn trust_update<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	document: &TrustDocument,
	old_tip: ObjectId<H>,
	label: &str,
	identity: &impl Identity,
	signer: &impl Signer,
) -> Result<ObjectId<H>> {
	let new_tip = write_trust_commit(
		repo,
		document,
		vec![old_tip],
		&format!("gitana trust: {label}"),
		identity,
		signer,
	)
	.await?;
	// Prove the new chain (signed by a key the *previous* root trusts, and a fast-forward of it)
	// before the ref moves — the same check receive-pack makes.
	verify_candidate_trust_update(repo, Some(old_tip), new_tip).await?;
	repo
		.refs()
		.update_ref(TRUST_REF, new_tip, Some(old_tip))
		.await?;
	let committer = identity.committer_or_default().await;
	repo
		.refs()
		.append_reflog(
			TRUST_REF,
			Some(old_tip),
			new_tip,
			&committer,
			&format!("trust: {label}"),
		)
		.await?;
	Ok(new_tip)
}

/// The current trust tip, or an error when trust is not initialised (there is nothing to update).
async fn current_tip<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<ObjectId<H>> {
	repo
		.refs()
		.resolve(TRUST_REF)
		.await?
		.ok_or_else(|| anyhow!("trust is not initialised; run `gta trust init` first"))
}

/// Read the current trust document — raw, preserving each key's exact line and any metadata — from
/// the trust commit `tip`'s tree, so an edit changes only what it means to.
async fn read_current_document<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	tip: ObjectId<H>,
) -> Result<TrustDocument> {
	let tree = repo.commit_tree(tip).await?;
	let entries = repo.read_tree(tree).await?;
	let (_, _, blob) = entries
		.iter()
		.find(|(path, _, _)| path == TRUST_DOCUMENT_PATH)
		.ok_or_else(|| anyhow!("trust commit {tip} has no {TRUST_DOCUMENT_PATH}"))?;
	let bytes = repo.read_blob(*blob).await?;
	TrustDocument::from_json(&bytes).map_err(Into::into)
}

/// Resolve a key selector to a fingerprint: a `SHA256:…` value is used as-is; anything else is parsed
/// as an OpenSSH public-key line and fingerprinted.
fn selector_fingerprint(selector: &str) -> Result<String> {
	let selector = selector.trim();
	if selector.starts_with("SHA256:") {
		Ok(selector.to_owned())
	} else {
		Ok(
			TrustedKey::from_openssh(selector)
				.context("parsing the key selector as an OpenSSH public key")?
				.id()
				.as_str()
				.to_owned(),
		)
	}
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use std::path::{Path, PathBuf};

	use gitana_file_store_local::LocalFileStore;
	use gitana_git_http::{ProtocolVersion, Service, advertise, upload_pack_v0};
	use gitana_object::Sha256;
	use gitana_object_store::ObjectStore;
	use gitana_trust::TrustedKey;

	use super::*;
	use crate::test_support::{TestIdentity, TestSigner, fixture, open_dir};

	/// An [`HttpTransport`] that serves a "server" repository's own `git-upload-pack` handlers
	/// in-process (advertisement + pack), so `trust_sync` can be exercised end to end without a socket.
	/// It reopens the repo per call (like the real server), so trust updates made between calls show up.
	struct ServerTransport {
		git_dir: PathBuf,
	}

	impl ServerTransport {
		fn open(&self) -> Repository<LocalFileStore, Sha256> {
			Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
				open_dir(&self.git_dir),
			)))
		}
	}

	impl HttpTransport for ServerTransport {
		async fn get(&self, _url: &str) -> Result<Vec<u8>> {
			Ok(advertise(&self.open(), Service::UploadPack, ProtocolVersion::V0, None).await?)
		}

		async fn post(&self, _url: &str, _content_type: &str, body: Vec<u8>) -> Result<Vec<u8>> {
			Ok(upload_pack_v0(&self.open(), &body).await?)
		}
	}

	/// A bare-ish server repo (initialised, no work tree) at a fresh temp dir, returning its git dir so
	/// a [`ServerTransport`] can reopen it, plus the repo handle to author trust commits through.
	async fn server() -> (
		tempfile::TempDir,
		PathBuf,
		Repository<LocalFileStore, Sha256>,
	) {
		let dir = tempfile::TempDir::new().unwrap();
		let git_dir = dir.path().join("srv.git");
		std::fs::create_dir_all(&git_dir).unwrap();
		let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
			open_dir(&git_dir),
		)));
		repo.init().await.unwrap();
		(dir, git_dir, repo)
	}

	/// The advertisement a [`ServerTransport`] would serve for `git_dir`.
	async fn advertisement(git_dir: &Path) -> Vec<u8> {
		let transport = ServerTransport {
			git_dir: git_dir.to_path_buf(),
		};
		transport.get("").await.unwrap()
	}

	/// A dummy origin — the [`ServerTransport`] ignores the URL and routes by handler.
	fn origin() -> Origin {
		Origin::parse("http://test.invalid/repo.git").unwrap()
	}

	#[tokio::test]
	async fn sync_adopts_a_verifying_remote_root_when_local_has_none() {
		let (_srv_dir, git_dir, server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let client = wt.repository();
		let signer = TestSigner::new(1);

		// The server bootstraps a signed trust root; the client has none.
		let server_tip = trust_init(
			&server_repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		let outcome = trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(true),
		)
		.await
		.unwrap();

		match outcome {
			TrustSyncOutcome::Updated { old: None, new } => assert_eq!(new, server_tip),
			_ => panic!("expected a bootstrap adoption"),
		}
		// The local ref now points at the server tip and folds back to the enrolled root.
		assert_eq!(
			client.refs().resolve(TRUST_REF).await.unwrap(),
			Some(server_tip)
		);
		let root = trust_list(client).await.unwrap().unwrap();
		assert_eq!(root.policy, Policy::Warn);
		assert_eq!(root.keys.len(), 1);
	}

	#[tokio::test]
	async fn sync_fast_forwards_local_trust() {
		let (_srv_dir, git_dir, server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let client = wt.repository();
		let (admin, colleague) = (TestSigner::new(1), TestSigner::new(2));

		trust_init(
			&server_repo,
			Policy::Warn,
			&admin.public_line(),
			false,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();

		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		// First sync: adopt the bootstrap.
		trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(true),
		)
		.await
		.unwrap();

		// The server enrols a second key (extending the chain), then the client syncs again.
		let server_tip = trust_add_key(
			&server_repo,
			&colleague.public_line(),
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();
		let outcome = trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(true),
		)
		.await
		.unwrap();

		match outcome {
			TrustSyncOutcome::Updated { old: Some(_), new } => assert_eq!(new, server_tip),
			_ => panic!("expected a fast-forward update"),
		}
		assert_eq!(trust_list(client).await.unwrap().unwrap().keys.len(), 2);
	}

	#[tokio::test]
	async fn sync_refuses_a_divergent_remote_root_and_leaves_the_local_ref() {
		let (_srv_dir, git_dir, server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let client = wt.repository();
		let (mine, theirs) = (TestSigner::new(1), TestSigner::new(2));

		// The client and server bootstrap *independent* roots (no shared history).
		let local_tip = trust_init(
			client,
			Policy::Warn,
			&mine.public_line(),
			false,
			&TestIdentity::default(),
			&mine,
		)
		.await
		.unwrap();
		trust_init(
			&server_repo,
			Policy::Warn,
			&theirs.public_line(),
			false,
			&TestIdentity::default(),
			&theirs,
		)
		.await
		.unwrap();

		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		let err = trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(true),
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("does not descend"), "{err}");
		// The divergent remote root was refused: the local ref never moved.
		assert_eq!(
			client.refs().resolve(TRUST_REF).await.unwrap(),
			Some(local_tip)
		);
	}

	#[tokio::test]
	async fn sync_is_a_noop_when_already_up_to_date() {
		let (_srv_dir, git_dir, server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let client = wt.repository();
		let signer = TestSigner::new(1);
		trust_init(
			&server_repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(true),
		)
		.await
		.unwrap();
		// A second sync with no server movement is a no-op.
		let outcome = trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(true),
		)
		.await
		.unwrap();
		assert!(matches!(outcome, TrustSyncOutcome::UpToDate));
	}

	#[tokio::test]
	async fn sync_reports_a_remote_without_a_trust_root() {
		let (_srv_dir, git_dir, _server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		let outcome = trust_sync(
			&transport,
			wt.repository(),
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(true),
		)
		.await
		.unwrap();
		assert!(matches!(outcome, TrustSyncOutcome::RemoteUnset));
	}

	#[tokio::test]
	async fn sync_declining_a_bootstrap_leaves_the_ref_unset() {
		let (_srv_dir, git_dir, server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let client = wt.repository();
		let signer = TestSigner::new(1);
		let server_tip = trust_init(
			&server_repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		// The confirm callback declines the (verifying but unseen) bootstrap root.
		let outcome = trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(false),
		)
		.await
		.unwrap();

		match outcome {
			TrustSyncOutcome::Declined { new } => assert_eq!(new, server_tip),
			_ => panic!("expected a declined bootstrap"),
		}
		// Declined: the local ref must stay unset.
		assert!(client.refs().resolve(TRUST_REF).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn sync_bootstrap_confirm_sees_the_signing_anchor_and_can_adopt() {
		let (_srv_dir, git_dir, server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let client = wt.repository();
		let signer = TestSigner::new(1);
		trust_init(
			&server_repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		// The confirm callback is handed the chain's anchor: the key that *signed* the bootstrap. A
		// caller pinning `--expect` compares against exactly this.
		let expected = fingerprint(&signer);
		let outcome = trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async move |root: &TrustRoot, anchor: &KeyId| {
				assert_eq!(anchor.as_str(), expected);
				assert_eq!(root.keys.len(), 1);
				Ok(true)
			},
		)
		.await
		.unwrap();
		assert!(matches!(
			outcome,
			TrustSyncOutcome::Updated { old: None, .. }
		));
	}

	#[tokio::test]
	async fn sync_bootstrap_confirm_error_propagates_and_leaves_the_ref_unset() {
		let (_srv_dir, git_dir, server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let client = wt.repository();
		let signer = TestSigner::new(1);
		trust_init(
			&server_repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		// A confirm that errors (e.g. a `--expect` fingerprint that does not match the anchor) aborts
		// the sync — the error propagates and the ref never moves.
		let err = trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| anyhow::bail!("anchor does not match --expect"),
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("--expect"), "{err}");
		assert!(client.refs().resolve(TRUST_REF).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn sync_fast_forward_never_invokes_confirm() {
		let (_srv_dir, git_dir, server_repo) = server().await;
		let (_dir, wt) = fixture().await;
		let client = wt.repository();
		let (admin, colleague) = (TestSigner::new(1), TestSigner::new(2));
		trust_init(
			&server_repo,
			Policy::Warn,
			&admin.public_line(),
			false,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();

		let transport = ServerTransport {
			git_dir: git_dir.clone(),
		};
		// First sync adopts the bootstrap (confirmed).
		trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| Ok(true),
		)
		.await
		.unwrap();

		// The server extends the chain; the client's second sync is a fast-forward anchored to the
		// already-trusted local root, so confirm must never be consulted.
		trust_add_key(
			&server_repo,
			&colleague.public_line(),
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();
		let outcome = trust_sync(
			&transport,
			client,
			&origin(),
			&advertisement(&git_dir).await,
			&TestIdentity::default(),
			async |_, _| panic!("confirm must not be called on a fast-forward"),
		)
		.await
		.unwrap();
		assert!(matches!(
			outcome,
			TrustSyncOutcome::Updated { old: Some(_), .. }
		));
	}

	#[tokio::test]
	async fn list_is_none_before_init() {
		let (_dir, wt) = fixture().await;
		assert!(trust_list(wt.repository()).await.unwrap().is_none());
	}

	#[tokio::test]
	async fn init_bootstraps_a_self_signed_root_that_folds_back() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let signer = TestSigner::new(1);

		let tip = trust_init(
			repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		// The ref points at the new commit, and folding it back yields the enrolled key and policy —
		// proving the signed bootstrap commit actually verifies through the trust core.
		assert_eq!(repo.refs().resolve(TRUST_REF).await.unwrap(), Some(tip));
		let root = trust_list(repo).await.unwrap().unwrap();
		assert_eq!(root.policy, Policy::Warn);
		assert_eq!(root.keys.len(), 1);
		let enrolled = TrustedKey::from_openssh(&signer.public_line()).unwrap();
		assert_eq!(root.keys[0].id(), enrolled.id());
	}

	#[tokio::test]
	async fn init_refuses_a_second_bootstrap() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let signer = TestSigner::new(1);
		trust_init(
			repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();

		let err = trust_init(
			repo,
			Policy::Warn,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("already initialised"), "{err}");
	}

	#[tokio::test]
	async fn init_refuses_require_with_a_single_key_unless_break_glass() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let signer = TestSigner::new(1);

		let err = trust_init(
			repo,
			Policy::Require,
			&signer.public_line(),
			false,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("break-glass"), "{err}");
		// The refusal left the ref unset, so break-glass can then bootstrap it.
		assert!(repo.refs().resolve(TRUST_REF).await.unwrap().is_none());

		trust_init(
			repo,
			Policy::Require,
			&signer.public_line(),
			true,
			&TestIdentity::default(),
			&signer,
		)
		.await
		.unwrap();
		let root = trust_list(repo).await.unwrap().unwrap();
		assert_eq!(root.policy, Policy::Require);
	}

	/// Bootstrap `signer`'s self-signed root under `policy` (break-glass, so tests can start at
	/// `require`), returning the repository's tip.
	async fn bootstrap(
		repo: &Repository<LocalFileStore, Sha256>,
		signer: &TestSigner,
		policy: Policy,
	) -> ObjectId<Sha256> {
		trust_init(
			repo,
			policy,
			&signer.public_line(),
			true,
			&TestIdentity::default(),
			signer,
		)
		.await
		.unwrap()
	}

	fn fingerprint(signer: &TestSigner) -> String {
		TrustedKey::from_openssh(&signer.public_line())
			.unwrap()
			.id()
			.as_str()
			.to_owned()
	}

	#[tokio::test]
	async fn add_key_enrols_a_second_key() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let (admin, colleague) = (TestSigner::new(1), TestSigner::new(2));
		bootstrap(repo, &admin, Policy::Warn).await;

		trust_add_key(
			repo,
			&colleague.public_line(),
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();

		let root = trust_list(repo).await.unwrap().unwrap();
		let ids: Vec<String> = root
			.keys
			.iter()
			.map(|k| k.id().as_str().to_owned())
			.collect();
		assert!(ids.contains(&fingerprint(&admin)) && ids.contains(&fingerprint(&colleague)));
	}

	#[tokio::test]
	async fn add_key_refuses_a_duplicate() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let admin = TestSigner::new(1);
		bootstrap(repo, &admin, Policy::Warn).await;

		let err = trust_add_key(repo, &admin.public_line(), &TestIdentity::default(), &admin)
			.await
			.unwrap_err();
		assert!(err.to_string().contains("already enrolled"), "{err}");
	}

	#[tokio::test]
	async fn add_key_signed_by_an_untrusted_key_is_refused_and_leaves_the_ref() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let (admin, colleague, outsider) = (TestSigner::new(1), TestSigner::new(2), TestSigner::new(3));
		let tip = bootstrap(repo, &admin, Policy::Warn).await;

		// `outsider` is not in the current root, so the update cannot verify.
		let err = trust_add_key(
			repo,
			&colleague.public_line(),
			&TestIdentity::default(),
			&outsider,
		)
		.await
		.unwrap_err();
		assert!(!err.to_string().is_empty());
		// The ref must not have moved.
		assert_eq!(repo.refs().resolve(TRUST_REF).await.unwrap(), Some(tip));
	}

	#[tokio::test]
	async fn remove_key_by_fingerprint() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let (admin, colleague) = (TestSigner::new(1), TestSigner::new(2));
		bootstrap(repo, &admin, Policy::Warn).await;
		trust_add_key(
			repo,
			&colleague.public_line(),
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();

		// Remove the original admin, signing with the still-trusted colleague.
		trust_remove_key(
			repo,
			&fingerprint(&admin),
			false,
			&TestIdentity::default(),
			&colleague,
		)
		.await
		.unwrap();

		let root = trust_list(repo).await.unwrap().unwrap();
		assert_eq!(root.keys.len(), 1);
		assert_eq!(root.keys[0].id().as_str(), fingerprint(&colleague));
	}

	#[tokio::test]
	async fn remove_key_refuses_unknown_and_last() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let admin = TestSigner::new(1);
		bootstrap(repo, &admin, Policy::Warn).await;

		let unknown = trust_remove_key(
			repo,
			"SHA256:AAAAdoesnotexist",
			false,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap_err();
		assert!(
			unknown.to_string().contains("no enrolled key matches"),
			"{unknown}"
		);

		let last = trust_remove_key(
			repo,
			&fingerprint(&admin),
			false,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap_err();
		assert!(last.to_string().contains("last trusted key"), "{last}");
	}

	#[tokio::test]
	async fn remove_key_refuses_dropping_require_below_two_keys_without_break_glass() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let (admin, colleague) = (TestSigner::new(1), TestSigner::new(2));
		bootstrap(repo, &admin, Policy::Warn).await;
		trust_add_key(
			repo,
			&colleague.public_line(),
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();
		trust_set_policy(
			repo,
			Policy::Require,
			false,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();

		// Removing one key would leave a single-key `require` root: refused without break-glass.
		let err = trust_remove_key(
			repo,
			&fingerprint(&colleague),
			false,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("break-glass"), "{err}");
		assert_eq!(trust_list(repo).await.unwrap().unwrap().keys.len(), 2);

		// With break-glass it proceeds.
		trust_remove_key(
			repo,
			&fingerprint(&colleague),
			true,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();
		assert_eq!(trust_list(repo).await.unwrap().unwrap().keys.len(), 1);
	}

	#[tokio::test]
	async fn set_policy_require_needs_two_keys() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let (admin, colleague) = (TestSigner::new(1), TestSigner::new(2));
		bootstrap(repo, &admin, Policy::Warn).await;

		// One key: refused without break-glass.
		let err = trust_set_policy(
			repo,
			Policy::Require,
			false,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap_err();
		assert!(err.to_string().contains("fewer than two"), "{err}");

		// Enrol a second key, then the flip succeeds.
		trust_add_key(
			repo,
			&colleague.public_line(),
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();
		trust_set_policy(
			repo,
			Policy::Require,
			false,
			&TestIdentity::default(),
			&admin,
		)
		.await
		.unwrap();
		assert_eq!(
			trust_list(repo).await.unwrap().unwrap().policy,
			Policy::Require
		);
	}

	#[tokio::test]
	async fn set_policy_refuses_a_noop() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let admin = TestSigner::new(1);
		bootstrap(repo, &admin, Policy::Warn).await;

		let err = trust_set_policy(repo, Policy::Warn, false, &TestIdentity::default(), &admin)
			.await
			.unwrap_err();
		assert!(err.to_string().contains("already"), "{err}");
	}

	#[tokio::test]
	async fn updates_require_an_initialised_root() {
		let (_dir, wt) = fixture().await;
		let repo = wt.repository();
		let admin = TestSigner::new(1);

		let err = trust_add_key(repo, &admin.public_line(), &TestIdentity::default(), &admin)
			.await
			.unwrap_err();
		assert!(err.to_string().contains("not initialised"), "{err}");
	}
}
