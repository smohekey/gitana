//! Merge-base (best common ancestor) computation and ancestry queries — the primitives the
//! merge / rebase / cherry-pick family build on.
//!
//! `merge_base` implements git's paint-down-to-common-ancestors over a committer-date-ordered heap
//! (the same ordering [`crate::revision`] uses for `rev-list`). The walks drain reachable history
//! rather than using git's early-stop / date-`slop` optimization: correct, and adequate at this
//! project's scale — performance tuning is deferred, like packed-object lookup elsewhere.

use std::collections::{BinaryHeap, HashMap, HashSet};

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId};

use crate::revision::{committer_seconds, peel_to_commit, read_commit};
use crate::{Repository, RepositoryError};

// Per-commit paint flags during a two-tip walk.
const PARENT1: u8 = 1; // reachable from the first tip
const PARENT2: u8 = 2; // reachable from the second tip
const STALE: u8 = 4; // an ancestor of an already-found common ancestor
const RESULT: u8 = 8; // already recorded as a candidate base

/// The best common ancestor(s) — merge bases — of `commits`, empty if they share no ancestor.
///
/// This is git's default (non-`--octopus`) semantics: the merge base of the **first** commit and
/// the **set** of the rest. A base must be reachable from the first commit and from *at least one*
/// of the others — it need not be common to every argument. (So `merge-base C B A` where `A` is an
/// ancestor of `C` and `B` is unrelated yields `A`.) For exactly two commits this is just their
/// common ancestor; the argument order matters only in that the first commit is the one all bases
/// must descend from.
///
/// Bases are returned newest-committer-date first (the date-priority walk discovers them in that
/// order), matching git's ordering. Among bases that share a commit date the relative order — and
/// hence git's single-base choice — is unspecified; ours may differ but is an equally valid base.
pub(crate) async fn merge_base<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	commits: &[ObjectId<H>],
) -> Result<Vec<ObjectId<H>>, RepositoryError> {
	let Some((first, rest)) = commits.split_first() else {
		return Ok(Vec::new());
	};
	let one = peel_to_commit(repo, *first).await?;
	let mut others = Vec::with_capacity(rest.len());
	for commit in rest {
		others.push(peel_to_commit(repo, *commit).await?);
	}
	let candidates = paint_down(repo, one, &others).await?;
	remove_redundant(repo, candidates).await
}

/// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
pub(crate) async fn is_ancestor<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	ancestor: ObjectId<H>,
	descendant: ObjectId<H>,
) -> Result<bool, RepositoryError> {
	let ancestor = peel_to_commit(repo, ancestor).await?;
	let descendant = peel_to_commit(repo, descendant).await?;

	let mut seen = HashSet::from([descendant]);
	let mut stack = vec![descendant];
	while let Some(id) = stack.pop() {
		if id == ancestor {
			return Ok(true);
		}
		for parent in read_commit(repo, id).await?.parents {
			if seen.insert(parent) {
				stack.push(parent);
			}
		}
	}
	Ok(false)
}

/// Paint history down from `one` (flag `PARENT1`) and every commit in `others` (flag `PARENT2`),
/// returning the common-ancestor candidates in discovery order. A commit reached from `one` and
/// from at least one of `others`, and not yet stale, is a candidate; recording it marks it stale so
/// its own ancestors are not recorded again. All inputs are already peeled to commits.
async fn paint_down<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	one: ObjectId<H>,
	others: &[ObjectId<H>],
) -> Result<Vec<ObjectId<H>>, RepositoryError> {
	let mut flags: HashMap<ObjectId<H>, u8> = HashMap::new();
	let mut heap: BinaryHeap<(i64, ObjectId<H>)> = BinaryHeap::new();

	flags.insert(one, PARENT1);
	heap.push((committer_seconds(repo, one).await?, one));
	for &other in others {
		let entry = flags.entry(other).or_insert(0);
		if *entry & PARENT2 == 0 {
			*entry |= PARENT2;
			heap.push((committer_seconds(repo, other).await?, other));
		}
	}

	let mut result = Vec::new();
	while let Some((_, id)) = heap.pop() {
		let carry = flags[&id] & (PARENT1 | PARENT2 | STALE);
		// Reached from both tips and not already an ancestor of a found base.
		let mut to_parents = carry;
		if carry == (PARENT1 | PARENT2) {
			let flag = flags.get_mut(&id).expect("popped commit was painted");
			if *flag & RESULT == 0 {
				*flag |= RESULT;
				result.push(id);
			}
			to_parents |= STALE;
		}
		for parent in read_commit(repo, id).await?.parents {
			let entry = flags.entry(parent).or_insert(0);
			if *entry & to_parents == to_parents {
				continue; // parent already carries these flags
			}
			*entry |= to_parents;
			heap.push((committer_seconds(repo, parent).await?, parent));
		}
	}
	Ok(result)
}

/// Drop any candidate that is an ancestor of another — in a criss-cross history `paint_down` can
/// surface several candidates, and only the maximal ones are true merge bases.
async fn remove_redundant<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	candidates: Vec<ObjectId<H>>,
) -> Result<Vec<ObjectId<H>>, RepositoryError> {
	let mut kept = Vec::new();
	for (i, candidate) in candidates.iter().enumerate() {
		let mut redundant = false;
		for (j, other) in candidates.iter().enumerate() {
			if i != j && is_ancestor(repo, *candidate, *other).await? {
				redundant = true;
				break;
			}
		}
		if !redundant {
			kept.push(*candidate);
		}
	}
	Ok(kept)
}

#[cfg(test)]
mod tests {
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::Sha256;
	use gitana_object_store::ObjectStore;

	use super::*;

	type Repo = Repository<MemoryFileStore, Sha256>;

	async fn new_repo() -> (Repo, ObjectId<Sha256>) {
		let repo = Repository::new(ObjectStore::new(MemoryFileStore::new()));
		let tree = repo.write_tree(&[]).await.unwrap();
		(repo, tree)
	}

	/// A commit with the given parents and committer time (the tree is irrelevant to ancestry).
	async fn commit(
		repo: &Repo,
		tree: ObjectId<Sha256>,
		parents: &[ObjectId<Sha256>],
		secs: i64,
	) -> ObjectId<Sha256> {
		let sig = format!("A U Thor <a@u> {secs} +0000");
		repo
			.create_commit(tree, parents.to_vec(), &sig, &sig, &format!("c{secs}\n"))
			.await
			.unwrap()
	}

	fn sorted(mut ids: Vec<ObjectId<Sha256>>) -> Vec<ObjectId<Sha256>> {
		ids.sort();
		ids
	}

	#[tokio::test]
	async fn linear_history() {
		let (repo, tree) = new_repo().await;
		let a = commit(&repo, tree, &[], 1).await;
		let b = commit(&repo, tree, &[a], 2).await;
		let c = commit(&repo, tree, &[b], 3).await;

		// The base of two commits on one line is the older one.
		assert_eq!(merge_base(&repo, &[c, b]).await.unwrap(), vec![b]);
		assert_eq!(merge_base(&repo, &[c, a]).await.unwrap(), vec![a]);
		assert_eq!(merge_base(&repo, &[a, a]).await.unwrap(), vec![a]);

		assert!(is_ancestor(&repo, a, c).await.unwrap());
		assert!(is_ancestor(&repo, a, a).await.unwrap());
		assert!(!is_ancestor(&repo, c, a).await.unwrap());
	}

	#[tokio::test]
	async fn fork_shares_the_root() {
		let (repo, tree) = new_repo().await;
		let root = commit(&repo, tree, &[], 1).await;
		let x = commit(&repo, tree, &[root], 2).await;
		let y = commit(&repo, tree, &[root], 3).await;

		assert_eq!(merge_base(&repo, &[x, y]).await.unwrap(), vec![root]);
		assert!(!is_ancestor(&repo, x, y).await.unwrap());
		assert!(is_ancestor(&repo, root, x).await.unwrap());
	}

	#[tokio::test]
	async fn unrelated_roots_have_no_base() {
		let (repo, tree) = new_repo().await;
		let r1 = commit(&repo, tree, &[], 1).await;
		let r2 = commit(&repo, tree, &[], 2).await;

		assert!(merge_base(&repo, &[r1, r2]).await.unwrap().is_empty());
		assert!(!is_ancestor(&repo, r1, r2).await.unwrap());
	}

	#[tokio::test]
	async fn through_a_merge_commit() {
		let (repo, tree) = new_repo().await;
		let root = commit(&repo, tree, &[], 1).await;
		let x = commit(&repo, tree, &[root], 2).await;
		let y = commit(&repo, tree, &[root], 3).await;
		let m = commit(&repo, tree, &[x, y], 4).await;

		// A parent of the merge is the base with that parent.
		assert_eq!(merge_base(&repo, &[m, x]).await.unwrap(), vec![x]);
		assert_eq!(merge_base(&repo, &[m, y]).await.unwrap(), vec![y]);
		assert!(is_ancestor(&repo, x, m).await.unwrap());
		// Octopus base of three commits.
		assert_eq!(merge_base(&repo, &[x, y, root]).await.unwrap(), vec![root]);
	}

	#[tokio::test]
	async fn multi_commit_uses_git_default_semantics() {
		let (repo, tree) = new_repo().await;
		let a = commit(&repo, tree, &[], 1).await; // a <- c
		let c = commit(&repo, tree, &[a], 2).await;
		let b = commit(&repo, tree, &[], 3).await; // unrelated root

		// Base of `c` against the set {b, a}: `a` is reachable from `c` and is one of the others, so
		// it is a base — even though it is not common to `b`. (git's `merge-base C B A`.)
		assert_eq!(merge_base(&repo, &[c, b, a]).await.unwrap(), vec![a]);
		// With `b` first, nothing but `b` is reachable from it, so there is no base.
		assert!(merge_base(&repo, &[b, c, a]).await.unwrap().is_empty());
	}

	#[tokio::test]
	async fn criss_cross_has_two_bases() {
		let (repo, tree) = new_repo().await;
		let root = commit(&repo, tree, &[], 1).await;
		let a = commit(&repo, tree, &[root], 2).await;
		let b = commit(&repo, tree, &[root], 3).await;
		let m1 = commit(&repo, tree, &[a, b], 4).await;
		let m2 = commit(&repo, tree, &[a, b], 5).await;

		// Both `a` and `b` are best common ancestors; `root` is redundant and dropped.
		assert_eq!(
			sorted(merge_base(&repo, &[m1, m2]).await.unwrap()),
			sorted(vec![a, b])
		);
	}
}
