//! The single seam through which every history operation records a commit, so SSH signing is applied
//! uniformly. `signer` is `Some` when the commit should be signed (`gta commit -S`, or a repo whose
//! git config requests signing) and `None` otherwise; when `Some`, the `gpgsig` armor covers the
//! exact bytes git signs, so the object verifies through stock git and the `gitana-trust` core alike.

use anyhow::Result;
use gitana_file_store::FileStore;
use gitana_object::{Commit, HashAlgorithm, ObjectId, ObjectKind, encode_commit};
use gitana_repository::Repository;

use crate::Signer;

/// Build a commit object from the given fields, sign it when `signer` is `Some`, and write it —
/// returning its id. Does not move any ref. The signature is computed over the unsigned encoding
/// (exactly what git signs); attaching the armor and re-encoding yields the final object, matching
/// `gitana_trust::verify_commit`, which strips the `gpgsig` header back off to check it.
pub(crate) async fn seal_commit<F: FileStore, H: HashAlgorithm, S: Signer>(
	repo: &Repository<F, H>,
	tree: ObjectId<H>,
	parents: Vec<ObjectId<H>>,
	author: &str,
	committer: &str,
	message: &str,
	signer: Option<&S>,
) -> Result<ObjectId<H>> {
	let mut commit = Commit {
		tree,
		parents,
		author: author.to_owned(),
		committer: committer.to_owned(),
		signature: None,
		extra_headers: Vec::new(),
		message: message.to_owned(),
	};
	if let Some(signer) = signer {
		commit.signature = Some(signer.sign(&encode_commit(&commit)).await?);
	}
	Ok(
		repo
			.objects()
			.write_object(ObjectKind::Commit, &encode_commit(&commit))
			.await?,
	)
}

/// Record an (optionally signed) commit on the branch `HEAD` points at — the signing-aware analog of
/// [`Repository::commit_on_head`], returning the new commit id. Resolves the parent up front so the
/// object that is signed is built on the same tip the ref update will CAS against. Used by every
/// HEAD-advancing operation (commit, cherry-pick, revert, rebase replay).
pub(crate) async fn commit_on_head<F: FileStore, H: HashAlgorithm, S: Signer>(
	repo: &Repository<F, H>,
	tree: ObjectId<H>,
	author: &str,
	committer: &str,
	message: &str,
	signer: Option<&S>,
) -> Result<ObjectId<H>> {
	let (target, parent) = repo.head_branch_tip().await?;
	let id = seal_commit(
		repo,
		tree,
		parent.map(|p| vec![p]).unwrap_or_default(),
		author,
		committer,
		message,
		signer,
	)
	.await?;
	repo
		.record_commit(&target, parent, id, committer, message)
		.await?;
	Ok(id)
}
