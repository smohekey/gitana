//! Server-side fetch negotiation shared by protocol v0 ([`crate::upload_pack_v0`]) and v2
//! ([`crate::fetch`]): which of the client's `have`s the server holds (the common cut points), and
//! whether the server can stop negotiating and build the pack (git's `ok_to_give_up`).
//!
//! Both transports are stateless over HTTP — each request re-sends the accumulated `have`s — so these
//! are pure functions of the request's `want`s/`have`s against the repository.

use std::collections::HashSet;

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

use crate::GitHttpError;
use crate::shallow::{peel_to_commit, read_commit};

/// The subset of `haves` the server actually has — the negotiation cut points a `multi_ack`/`ready`
/// response acknowledges as `common`.
pub(crate) async fn common_haves<H: HashAlgorithm>(
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

/// Whether the server can stop negotiating and build the pack (git's `ok_to_give_up`): there is at
/// least one common `have`, and **every** `want` has a `common` among its ancestors — so no further
/// `have` the client could send would trim the pack. It stays `false` while a want has no common
/// ancestor yet (a still-divergent or disjoint history), so the server keeps acknowledging until the
/// client either reveals a deeper common or ends the round with `done`.
pub(crate) async fn ok_to_give_up<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	commons: &[ObjectId<H>],
) -> Result<bool, GitHttpError> {
	if commons.is_empty() {
		return Ok(false);
	}
	let common: HashSet<ObjectId<H>> = commons.iter().copied().collect();
	for &want in wants {
		if !want_reaches_common(repo, want, &common).await? {
			return Ok(false);
		}
	}
	Ok(true)
}

/// Whether `want` (or, if it is a tag, the commit it peels to) has a `common` commit among its
/// ancestors — i.e. the client already holds a base from which the want is reachable.
async fn want_reaches_common<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	want: ObjectId<H>,
	common: &HashSet<ObjectId<H>>,
) -> Result<bool, GitHttpError> {
	// A want the client already has (the ref did not move) is trivially satisfied.
	if common.contains(&want) {
		return Ok(true);
	}
	let Some(start) = peel_to_commit(repo, want).await? else {
		// A non-commit want (a tree/blob want, or an absent one) has no commit ancestry to cut at.
		return Ok(false);
	};
	let mut stack = vec![start];
	let mut seen: HashSet<ObjectId<H>> = HashSet::new();
	while let Some(id) = stack.pop() {
		if common.contains(&id) {
			return Ok(true);
		}
		if !seen.insert(id) {
			continue;
		}
		if let Some(commit) = read_commit(repo, id).await? {
			stack.extend(commit.parents);
		}
	}
	Ok(false)
}

#[cfg(test)]
mod tests {
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::Sha256;
	use gitana_object_store::ObjectStore;

	use super::*;

	type Repo = Repository<MemoryFileStore, Sha256>;

	async fn new_repo() -> Repo {
		Repository::new(ObjectStore::new(MemoryFileStore::new()))
	}

	/// A commit with a distinct message so sibling roots do not collide into one oid.
	async fn commit(repo: &Repo, parents: &[ObjectId<Sha256>], msg: &str) -> ObjectId<Sha256> {
		let tree = repo.write_tree(&[]).await.unwrap();
		let who = "A U Thor <a@u> 0 +0000";
		repo
			.create_commit(tree, parents.to_vec(), who, who, msg)
			.await
			.unwrap()
	}

	#[tokio::test]
	async fn no_common_is_never_ready() {
		let repo = new_repo().await;
		let a = commit(&repo, &[], "a\n").await;
		let b = commit(&repo, &[a], "b\n").await;
		// The client offers a have the server does not have — no common, never ready.
		let stranger = ObjectId::<Sha256>::compute(gitana_object::ObjectKind::Commit, b"x");
		let commons = common_haves(&repo, &[stranger]).await.unwrap();
		assert!(commons.is_empty());
		assert!(!ok_to_give_up(&repo, &[b], &commons).await.unwrap());
	}

	#[tokio::test]
	async fn common_ancestor_of_every_want_is_ready() {
		let repo = new_repo().await;
		let base = commit(&repo, &[], "base\n").await;
		let tip = commit(&repo, &[base], "tip\n").await; // want, descends from base
		// The client has `base` (a common ancestor of the want) → ready.
		let commons = common_haves(&repo, &[base]).await.unwrap();
		assert_eq!(commons, vec![base]);
		assert!(ok_to_give_up(&repo, &[tip], &commons).await.unwrap());
	}

	#[tokio::test]
	async fn a_want_with_no_common_ancestor_is_not_ready() {
		let repo = new_repo().await;
		let base = commit(&repo, &[], "base\n").await;
		let tracked = commit(&repo, &[base], "tracked\n").await;
		// A second, disjoint root the client shares nothing with.
		let disjoint = commit(&repo, &[], "disjoint\n").await;
		// The client has `base` (a common for the first want) but nothing under the disjoint want, so the
		// server must keep negotiating: not every want is covered.
		let commons = common_haves(&repo, &[base]).await.unwrap();
		assert!(ok_to_give_up(&repo, &[tracked], &commons).await.unwrap());
		assert!(
			!ok_to_give_up(&repo, &[tracked, disjoint], &commons)
				.await
				.unwrap()
		);
	}

	#[tokio::test]
	async fn a_want_the_client_already_has_is_ready() {
		let repo = new_repo().await;
		let a = commit(&repo, &[], "a\n").await;
		// The want is itself a have (the ref did not move on the server).
		let commons = common_haves(&repo, &[a]).await.unwrap();
		assert!(ok_to_give_up(&repo, &[a], &commons).await.unwrap());
	}
}
