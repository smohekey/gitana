//! `amend`: replace the last commit with one made from the staged index, as `git commit --amend`.

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_identity::Identity;
use gitana_object::{Commit, HashAlgorithm, ObjectId, ObjectKind, encode_commit, parse_commit};
use gitana_repository::{ReflogIntent, RepositoryError};

use crate::{AmendError, WorkTree};

/// Refuse as git refuses (an operation in progress, unmerged paths, no commit to amend, a result
/// that would be empty), and if `HEAD` doesn't lead along `chain` to `expected`, before resolving
/// identity or writing anything; then write the commit (the replaced one's parents as git sees
/// them, its author and other headers, `identity` as committer) and move the ref at the end of
/// `chain` off the replaced commit by compare-and-set, along `chain` still: the branch, or `HEAD`
/// itself when detached.
pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	chain: &[String],
	expected: ObjectId<H>,
	message: &str,
	identity: &impl Identity,
) -> Result<ObjectId<H>, AmendError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	if let Some(operation) = repo
		.operation_in_progress()
		.await
		.map_err(AmendError::Repository)?
	{
		return Err(AmendError::InProgress(operation));
	}
	let index = wt.load_index().await.map_err(AmendError::Index)?;
	if index.has_conflicts() {
		return Err(AmendError::Unmerged);
	}
	// The commit replaced: where HEAD leads, through any chain of symbolic refs, the way the caller
	// saw it lead and to the commit they mean, or nothing is done. The chain is held to again when
	// the ref at its end moves.
	let refs = repo.refs();
	let now = refs
		.symbolic_chain("HEAD")
		.await
		.map_err(AmendError::Repository)?;
	let end = now.last().expect("a chain holds the ref it starts at");
	let Some(replaced) = refs.resolve(end).await.map_err(AmendError::Repository)? else {
		return Err(AmendError::NothingToAmend);
	};
	if now != chain || replaced != expected {
		return Err(AmendError::Moved);
	}
	let (kind, payload) = repo
		.objects()
		.read_object(&replaced)
		.await
		.map_err(|error| AmendError::Repository(error.into()))?;
	if kind != ObjectKind::Commit {
		return Err(AmendError::Repository(RepositoryError::InvalidRef(
			format!("{replaced} is not a commit"),
		)));
	}
	// Its header only: the message is replaced, and needn't be UTF-8 (one under an `encoding`
	// header needn't be).
	let header = match payload.windows(2).position(|pair| pair == b"\n\n") {
		Some(at) => &payload[..at + 2],
		None => &payload[..],
	};
	let old = parse_commit::<H>(header).map_err(|error| AmendError::Repository(error.into()))?;
	// Its parents as git sees them: none at the shallow boundary, where they're deliberately absent
	// (so git's amend there makes a commit with none).
	let parents = if repo
		.read_shallow()
		.await
		.map_err(AmendError::Repository)?
		.contains(&replaced)
	{
		Vec::new()
	} else {
		old.parents
	};
	let entries = index.committed_entries();
	let tree = repo
		.write_tree(&entries)
		.await
		.map_err(AmendError::Repository)?;
	// Empty as git judges it, for a commit of one parent or none: that parent's tree again, or an
	// empty tree. A merge is never empty: it records what it merged.
	let empty = match parents.as_slice() {
		[] => entries.is_empty(),
		[parent] => {
			repo
				.commit_tree(*parent)
				.await
				.map_err(AmendError::Repository)?
				== tree
		}
		_ => false,
	};
	if empty {
		return Err(AmendError::WouldBeEmpty);
	}
	let committer = identity.committer().await.map_err(AmendError::Identity)?;
	let message = if message.ends_with('\n') {
		message.to_owned()
	} else {
		format!("{message}\n")
	};
	// As git's amend: the replaced commit's other headers kept (a merged signed tag's `mergetag`), but
	// not its signatures (`gpgsig`, and `gpgsig-sha256`, either of which can sit among the others
	// here), which the new commit doesn't have, nor its `encoding`, the new message being UTF-8.
	let extra_headers = old
		.extra_headers
		.into_iter()
		.filter(|(name, _)| !matches!(name.as_str(), "encoding" | "gpgsig" | "gpgsig-sha256"))
		.collect();
	let commit = Commit {
		tree,
		parents,
		author: old.author,
		committer: committer.clone(),
		signature: None,
		extra_headers,
		message: message.clone(),
	};
	let commit = repo
		.objects()
		.write_object(ObjectKind::Commit, &encode_commit(&commit))
		.await
		.map_err(|error| AmendError::Repository(error.into()))?;
	let subject = message.lines().next().unwrap_or("");
	let reflog = format!("commit (amend): {subject}");
	// The ref at the end of the caller's chain moves (or HEAD itself, detached), each symbolic ref on
	// the way logged as git logs them. Should HEAD lead another way by now (switched to another
	// branch, even one at the same commit), or that ref no longer hold the commit replaced, nothing
	// moves.
	refs
		.update_ref_along(
			chain,
			commit,
			Some(replaced),
			ReflogIntent::Log {
				committer: &committer,
				message: &reflog,
			},
		)
		.await
		.map_err(|error| match error {
			RepositoryError::RefMoved { .. } => AmendError::Moved,
			other => AmendError::Repository(other),
		})?;
	Ok(commit)
}
