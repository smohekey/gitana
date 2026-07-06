use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, parse_commit, parse_tree};

use crate::{FoldedTrust, ObjectSource, TRUST_DOCUMENT_PATH, TrustError, TrustRoot, verify_commit};

/// Fold the `refs/gitana/trust` chain ending at `tip` into its effective [`TrustRoot`], verifying
/// the whole authorization chain: the bootstrap (root) commit must be self-signed by a key in its
/// own document, and every later commit must be signed by a key trusted in the *previous* root.
/// Returns the tip's root. The chain must be linear (a merge in it is refused) and every root
/// non-empty.
///
/// An absent trust ref means "no trust configured" — that is the caller's check (don't call this),
/// not an outcome here.
pub async fn fold_trust_root<H, S>(source: &S, tip: ObjectId<H>) -> Result<TrustRoot, TrustError>
where
	H: HashAlgorithm,
	S: ObjectSource<H>,
{
	Ok(fold_trust_root_anchored(source, tip).await?.root)
}

/// Like [`fold_trust_root`], but additionally surfaces the [`KeyId`](crate::KeyId) that signed the
/// chain's bootstrap commit as the [`FoldedTrust::anchor`]. Use this when adopting a never-before-seen
/// root and you need to pin *who* the chain's authority rests on — a key listed in the root is not
/// enough (see [`FoldedTrust`]).
pub async fn fold_trust_root_anchored<H, S>(
	source: &S,
	tip: ObjectId<H>,
) -> Result<FoldedTrust, TrustError>
where
	H: HashAlgorithm,
	S: ObjectSource<H>,
{
	// Collect the chain tip → bootstrap (each entry is the commit's raw bytes and its tree id),
	// then verify it bootstrap-first so each link is checked against the root that authorized it.
	let mut chain = Vec::new();
	let mut next = Some(tip);
	while let Some(id) = next {
		let (raw, parents, tree) = read_trust_commit(source, &id).await?;
		next = parents.into_iter().next();
		chain.push((raw, tree));
	}
	chain.reverse();

	let mut iter = chain.into_iter();
	let (bootstrap_raw, bootstrap_tree) = iter.next().expect("chain has at least the tip commit");
	// Bootstrap: self-signed by a key in its own root. The signer is the chain's anchor.
	let mut root = load_trust_root(source, bootstrap_tree).await?;
	let anchor = verify_commit::<H>(&bootstrap_raw, &root.keys)?;

	// Each later commit is authorized by the previous root, then installs its own.
	for (raw, tree) in iter {
		verify_commit::<H>(&raw, &root.keys)?;
		root = load_trust_root(source, tree).await?;
	}
	Ok(FoldedTrust { root, anchor })
}

/// Verify a candidate trust-root update from `old_tip` (the current ref, `None` at bootstrap) to
/// `new_tip`, **without moving any ref**, returning the new folded root. The new chain must be
/// internally valid (as [`fold_trust_root`]) and, when replacing an existing root, must extend it —
/// `old_tip` must be an ancestor of `new_tip`, so trust history is appended, never rewritten (a
/// divergent update is refused for manual reconciliation).
pub async fn verify_candidate_trust_update<H, S>(
	source: &S,
	old_tip: Option<ObjectId<H>>,
	new_tip: ObjectId<H>,
) -> Result<TrustRoot, TrustError>
where
	H: HashAlgorithm,
	S: ObjectSource<H>,
{
	Ok(
		verify_candidate_trust_update_anchored(source, old_tip, new_tip)
			.await?
			.root,
	)
}

/// Like [`verify_candidate_trust_update`], but additionally surfaces the chain's bootstrap
/// [`FoldedTrust::anchor`]. Use this on the bootstrap-adoption path (`old_tip` is `None`) to pin the
/// anchor before adopting an unseen root.
pub async fn verify_candidate_trust_update_anchored<H, S>(
	source: &S,
	old_tip: Option<ObjectId<H>>,
	new_tip: ObjectId<H>,
) -> Result<FoldedTrust, TrustError>
where
	H: HashAlgorithm,
	S: ObjectSource<H>,
{
	let folded = fold_trust_root_anchored(source, new_tip).await?;
	if let Some(old_tip) = old_tip
		&& !is_ancestor(source, &old_tip, new_tip).await?
	{
		return Err(TrustError::TrustChain(format!(
			"candidate trust update {new_tip} does not descend from current {old_tip}"
		)));
	}
	Ok(folded)
}

/// Read a trust-chain commit: its raw bytes (for signature verification), its parents, and its tree
/// id. A merge (more than one parent) is refused — the trust chain is linear.
async fn read_trust_commit<H, S>(
	source: &S,
	id: &ObjectId<H>,
) -> Result<(Vec<u8>, Vec<ObjectId<H>>, ObjectId<H>), TrustError>
where
	H: HashAlgorithm,
	S: ObjectSource<H>,
{
	let (kind, raw) = read(source, id).await?;
	if kind != ObjectKind::Commit {
		return Err(TrustError::TrustChain(format!(
			"trust ref object {id} is not a commit"
		)));
	}
	let commit = parse_commit::<H>(&raw)
		.map_err(|error| TrustError::TrustChain(format!("trust commit {id}: {error}")))?;
	if commit.parents.len() > 1 {
		return Err(TrustError::TrustChain(format!(
			"trust commit {id} is a merge; the trust chain must be linear"
		)));
	}
	Ok((raw, commit.parents, commit.tree))
}

/// Load the [`TrustRoot`] from a trust commit's `tree`: read the tree, find the `trust.json` blob,
/// and parse it.
async fn load_trust_root<H, S>(source: &S, tree: ObjectId<H>) -> Result<TrustRoot, TrustError>
where
	H: HashAlgorithm,
	S: ObjectSource<H>,
{
	let (kind, tree_bytes) = read(source, &tree).await?;
	if kind != ObjectKind::Tree {
		return Err(TrustError::TrustChain(format!(
			"trust object {tree} is not a tree"
		)));
	}
	let entries = parse_tree::<H>(&tree_bytes)
		.map_err(|error| TrustError::TrustChain(format!("trust tree {tree}: {error}")))?;
	let entry = entries
		.iter()
		.find(|entry| entry.name == TRUST_DOCUMENT_PATH)
		.ok_or_else(|| {
			TrustError::TrustChain(format!("trust tree {tree} has no {TRUST_DOCUMENT_PATH}"))
		})?;
	let (kind, blob) = read(source, &entry.id).await?;
	if kind != ObjectKind::Blob {
		return Err(TrustError::TrustChain(format!(
			"{TRUST_DOCUMENT_PATH} in tree {tree} is not a blob"
		)));
	}
	TrustRoot::from_json(&blob)
}

/// Whether `ancestor` is reachable from `tip` by following parents (the trust chain is linear, so a
/// straight walk suffices).
async fn is_ancestor<H, S>(
	source: &S,
	ancestor: &ObjectId<H>,
	tip: ObjectId<H>,
) -> Result<bool, TrustError>
where
	H: HashAlgorithm,
	S: ObjectSource<H>,
{
	let mut next = Some(tip);
	while let Some(id) = next {
		if &id == ancestor {
			return Ok(true);
		}
		let (_, parents, _) = read_trust_commit(source, &id).await?;
		next = parents.into_iter().next();
	}
	Ok(false)
}

/// Read an object through the source, mapping the backend error into [`TrustError::ObjectSource`].
async fn read<H, S>(source: &S, id: &ObjectId<H>) -> Result<(ObjectKind, Vec<u8>), TrustError>
where
	H: HashAlgorithm,
	S: ObjectSource<H>,
{
	source
		.read_object(id)
		.await
		.map_err(|error| TrustError::ObjectSource {
			id: id.to_hex(),
			source: Box::new(error),
		})
}
