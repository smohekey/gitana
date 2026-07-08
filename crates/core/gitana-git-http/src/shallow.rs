//! Computing a shallow-fetch boundary server-side: given the client's `want`s, its `deepen*` directive
//! and its current `shallow` set, decide which commits to send and which sit at the (new) history
//! boundary — the commits whose parents are deliberately withheld.
//!
//! A commit is a *boundary* commit when at least one of its parents is not included in the shallow view
//! (the depth limit was reached, the parent falls outside `deepen-since`, it is in the `deepen-not`
//! ancestor closure, or it is simply absent). A root commit (no parents) is never shallow. The pack
//! walk ([`crate::pack`]) stops at boundary commits, and the response advertises them with `shallow`
//! lines (and `unshallow` lines for commits the client had truncated whose parents are now sent).

use std::collections::{HashSet, VecDeque};

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, Signature, parse_commit, parse_tag};
use gitana_repository::Repository;

use crate::GitHttpError;
use crate::deepen::Deepen;

/// git's `INFINITE_DEPTH` sentinel — the absolute `deepen` a client sends for `fetch --unshallow`
/// (request all history). The server treats it as "drop every shallow boundary", not a literal depth.
const INFINITE_DEPTH: u32 = 0x7fff_ffff;

/// The outcome of a shallow-boundary computation.
pub(crate) struct ShallowPlan<H: HashAlgorithm> {
	/// The commits inside the shallow view (their tree closure is sent; boundary commits included).
	pub included: HashSet<ObjectId<H>>,
	/// The included commits whose parents are withheld — the new shallow boundary; the pack walk stops
	/// at these.
	pub boundary: HashSet<ObjectId<H>>,
	/// `shallow <oid>` lines to emit: boundary commits the client did not already list as shallow.
	pub shallow: Vec<ObjectId<H>>,
	/// `unshallow <oid>` lines to emit: commits the client had truncated whose parents are now sent.
	pub unshallow: Vec<ObjectId<H>>,
	/// Extra roots to seed the pack walk with — the now-included parents of the client's shallow
	/// commits. The wants (the client's tips) do not reach these on their own, so a deepen /
	/// `--unshallow` needs them to actually send the newly-exposed history.
	pub send_roots: Vec<ObjectId<H>>,
}

/// Compute the shallow view for `wants` under `deepen`, given the client's current `client_shallow`
/// boundary. Only commit `want`s drive the ancestry walk; a non-commit want (e.g. a tag) is left to the
/// pack walk and never becomes a boundary. An unknown want is ignored here (the caller validates wants).
pub(crate) async fn compute_shallow<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	deepen: &Deepen,
	client_shallow: &[ObjectId<H>],
) -> Result<ShallowPlan<H>, GitHttpError> {
	let excluded = deepen_not_closure(repo, &deepen.not).await?;
	let client: HashSet<ObjectId<H>> = client_shallow.iter().copied().collect();

	let mut included: HashSet<ObjectId<H>> = HashSet::new();
	let mut boundary: HashSet<ObjectId<H>> = HashSet::new();
	// Breadth-first over commit→parent edges. Each node carries a `budget`: the number of parent
	// descents still allowed (`None` = unlimited, bounded only by since/deepen-not). A want is peeled to
	// its commit first, so `--depth 1 <annotated-tag>` bounds the tag's commit, not its full ancestry.
	//
	// Absolute `deepen N` starts each tip at budget `N-1` (depth 1 = the tips alone). `deepen-relative N`
	// (`git fetch --deepen`) instead measures from the client's shallow frontier: above it the budget is
	// unlimited (the client already has that history), and each client-shallow commit re-seeds its
	// parents' budget to `N`, so `N` more levels are sent below the current boundary.
	let seed_budget: Option<u32> = if deepen.relative {
		None
	} else {
		deepen.depth.map(|n| n.saturating_sub(1))
	};
	let mut queue: VecDeque<(ObjectId<H>, Option<u32>)> = VecDeque::new();
	for &want in wants {
		if let Some(commit) = peel_to_commit(repo, want).await? {
			queue.push_back((commit, seed_budget));
		}
	}
	// `--unshallow` (an unbounded absolute `deepen`) drops *all* shallowness, so complete every commit the
	// client listed as shallow — not just those reachable from the wants. A narrowed/negative refspec may
	// leave a client shallow at a branch the wants no longer cover; git still unshallows it from the
	// client's `shallow` lines, so seed the walk from them too (unbounded), letting them be included,
	// unshallowed, and their now-exposed history sent. A finite `deepen N` re-depths only the wants (git
	// leaves the other branches' boundaries untouched), so this seeding is confined to the unshallow case.
	if !deepen.relative && deepen.depth == Some(INFINITE_DEPTH) {
		for &oid in client_shallow {
			queue.push_back((oid, None));
		}
	}
	while let Some((id, popped_budget)) = queue.pop_front() {
		if !included.insert(id) {
			continue;
		}
		let Some(commit) = read_commit(repo, id).await? else {
			continue; // not a commit (already peeled, so unreachable in practice)
		};
		// Relative deepening: crossing the client's frontier re-seeds the budget to `N` levels below it.
		let budget = if deepen.relative && client.contains(&id) {
			deepen.depth
		} else {
			popped_budget
		};
		let mut withholds_a_parent = false;
		for &parent in &commit.parents {
			// A parent is followed only while the depth budget allows it, and it passes the since /
			// deepen-not filters and exists — otherwise this commit sits at the shallow boundary.
			let within_budget = match budget {
				Some(0) => false,
				_ => true,
			};
			if within_budget
				&& parent_within_since(repo, parent, deepen.since).await?
				&& !excluded.contains(&parent)
				&& repo.objects().exists_object(&parent).await?
			{
				queue.push_back((parent, budget.map(|b| b - 1)));
			} else {
				withholds_a_parent = true;
			}
		}
		if withholds_a_parent {
			boundary.insert(id);
		}
	}

	let shallow = boundary
		.iter()
		.copied()
		.filter(|oid| !client.contains(oid))
		.collect();
	// A client-shallow commit becomes unshallow when it is in the view and no longer a boundary — i.e.
	// its parents are now being sent.
	let unshallow: Vec<ObjectId<H>> = client_shallow
		.iter()
		.copied()
		.filter(|oid| included.contains(oid) && !boundary.contains(oid))
		.collect();
	// The now-included parents of every client-shallow commit: the roots of the newly-exposed history
	// the pack must send (the client's tips, which we `want`, do not reach below its old boundary).
	let mut send_roots = Vec::new();
	for &oid in &unshallow {
		if let Some(commit) = read_commit(repo, oid).await? {
			for &parent in &commit.parents {
				if included.contains(&parent) {
					send_roots.push(parent);
				}
			}
		}
	}

	Ok(ShallowPlan {
		included,
		boundary,
		shallow,
		unshallow,
		send_roots,
	})
}

/// The tag objects to add to a shallow pack for `include-tag`: the id of each `refs/tags/*` whose
/// peeled commit target lies within the shallow view (`included`). A lightweight tag's id is the commit
/// itself (already sent, harmlessly deduped by the pack walk); an annotated tag's id is the tag object,
/// which the single-branch shallow client would otherwise never receive.
pub(crate) async fn reachable_tag_wants<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	included: &HashSet<ObjectId<H>>,
) -> Result<Vec<ObjectId<H>>, GitHttpError> {
	let mut wants = Vec::new();
	for (_, oid) in repo.refs().list("refs/tags/").await? {
		if let Some(commit) = peel_to_commit(repo, oid).await?
			&& included.contains(&commit)
		{
			wants.push(oid);
		}
	}
	Ok(wants)
}

/// The commits reachable from `wants` (peeled) but not from `haves` — the commit ancestry the pack
/// covers. Used to select `include-tag` tags for a *non-shallow* fetch (the shallow path already has an
/// `included` set). Walks commit→parent edges only, so it is cheaper than the full object walk.
pub(crate) async fn reachable_commits<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
) -> Result<HashSet<ObjectId<H>>, GitHttpError> {
	let mut excluded: HashSet<ObjectId<H>> = HashSet::new();
	let mut stack: Vec<ObjectId<H>> = haves.to_vec();
	while let Some(id) = stack.pop() {
		if !excluded.insert(id) {
			continue;
		}
		if let Some(commit) = read_commit(repo, id).await? {
			stack.extend(commit.parents);
		}
	}
	let mut included: HashSet<ObjectId<H>> = HashSet::new();
	let mut stack: Vec<ObjectId<H>> = Vec::new();
	for &want in wants {
		if let Some(commit) = peel_to_commit(repo, want).await? {
			stack.push(commit);
		}
	}
	while let Some(id) = stack.pop() {
		if excluded.contains(&id) || !included.insert(id) {
			continue;
		}
		if let Some(commit) = read_commit(repo, id).await? {
			stack.extend(commit.parents);
		}
	}
	Ok(included)
}

/// Peel a (possibly annotated, possibly chained) tag to the commit it names, or `None` if it does not
/// resolve to a commit.
pub(crate) async fn peel_to_commit<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	mut id: ObjectId<H>,
) -> Result<Option<ObjectId<H>>, GitHttpError> {
	loop {
		match repo.objects().read_object(&id).await {
			Ok((ObjectKind::Commit, _)) => return Ok(Some(id)),
			Ok((ObjectKind::Tag, data)) => id = parse_tag::<H>(&data)?.object,
			Ok(_) => return Ok(None),
			Err(gitana_object_store::ObjectStoreError::NotFound) => return Ok(None),
			Err(other) => return Err(other.into()),
		}
	}
}

/// Read `id` as a commit, or `None` when it is absent or not a commit.
pub(crate) async fn read_commit<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	id: ObjectId<H>,
) -> Result<Option<gitana_object::Commit<H>>, GitHttpError> {
	match repo.objects().read_object(&id).await {
		Ok((ObjectKind::Commit, data)) => Ok(Some(parse_commit::<H>(&data)?)),
		Ok(_) => Ok(None),
		Err(gitana_object_store::ObjectStoreError::NotFound) => Ok(None),
		Err(other) => Err(other.into()),
	}
}

/// Whether `parent`'s committer time is at or after `since` (git's `deepen-since`). `None` (no
/// `deepen-since`) always passes; an absent or non-commit parent never does.
async fn parent_within_since<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	parent: ObjectId<H>,
	since: Option<i64>,
) -> Result<bool, GitHttpError> {
	let Some(since) = since else {
		return Ok(true);
	};
	match read_commit(repo, parent).await? {
		Some(commit) => {
			let seconds = Signature::parse(&commit.committer)
				.map(|sig| sig.seconds)
				.unwrap_or(i64::MIN);
			Ok(seconds >= since)
		}
		None => Ok(false),
	}
}

/// The ancestor closure of the `deepen-not` refs/oids: every commit that must be excluded from the
/// shallow view. Each token is resolved as a hex oid or a ref name (tags peeled to their target); a
/// token that resolves to nothing is skipped.
async fn deepen_not_closure<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	not: &[String],
) -> Result<HashSet<ObjectId<H>>, GitHttpError> {
	let mut stack: Vec<ObjectId<H>> = Vec::new();
	for token in not {
		// git upload-pack rejects an unresolvable `deepen-not` ("deepen-not is not a ref") rather than
		// silently returning more history than asked for — a typo or stale ref must fail.
		let oid = resolve_ref_or_oid(repo, token)
			.await?
			.ok_or_else(|| GitHttpError::MalformedRequest(format!("deepen-not is not a ref: {token}")))?;
		stack.push(oid);
	}
	let mut closure: HashSet<ObjectId<H>> = HashSet::new();
	while let Some(id) = stack.pop() {
		if !closure.insert(id) {
			continue;
		}
		match repo.objects().read_object(&id).await {
			Ok((ObjectKind::Commit, data)) => stack.extend(parse_commit::<H>(&data)?.parents),
			Ok((ObjectKind::Tag, data)) => stack.push(parse_tag::<H>(&data)?.object),
			Ok(_) => {}
			Err(gitana_object_store::ObjectStoreError::NotFound) => {}
			Err(other) => return Err(other.into()),
		}
	}
	Ok(closure)
}

/// Resolve a `deepen-not` token: a full hex oid, else a ref name resolved against the repository. A
/// short name (git sends `--shallow-exclude`'s value verbatim, e.g. `mark`) is tried against git's DWIM
/// prefixes (`refs/tags/…`, `refs/heads/…`).
async fn resolve_ref_or_oid<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	token: &str,
) -> Result<Option<ObjectId<H>>, GitHttpError> {
	if let Ok(oid) = ObjectId::from_hex(token) {
		// A full oid must actually be present, else it excludes nothing and returns too much history;
		// `None` here surfaces as the same "deepen-not is not a ref" rejection as an unknown ref name.
		return Ok(repo.objects().exists_object(&oid).await?.then_some(oid));
	}
	let candidates: Vec<String> = if token.starts_with("refs/") {
		vec![token.to_owned()]
	} else {
		vec![
			token.to_owned(),
			format!("refs/{token}"),
			format!("refs/tags/{token}"),
			format!("refs/heads/{token}"),
			format!("refs/remotes/{token}"),
		]
	};
	for candidate in candidates {
		if let Some(oid) = repo.refs().resolve(&candidate).await? {
			return Ok(Some(oid));
		}
	}
	Ok(None)
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

	/// A commit with the given parents and committer time (an empty tree; ancestry is what matters).
	async fn commit(repo: &Repo, parents: &[ObjectId<Sha256>], secs: i64) -> ObjectId<Sha256> {
		let tree = repo.write_tree(&[]).await.unwrap();
		let sig = format!("A U Thor <a@u> {secs} +0000");
		repo
			.create_commit(tree, parents.to_vec(), &sig, &sig, &format!("c{secs}\n"))
			.await
			.unwrap()
	}

	fn set(ids: &[ObjectId<Sha256>]) -> HashSet<ObjectId<Sha256>> {
		ids.iter().copied().collect()
	}

	async fn plan(
		repo: &Repo,
		wants: &[ObjectId<Sha256>],
		deepen: Deepen,
		client_shallow: &[ObjectId<Sha256>],
	) -> ShallowPlan<Sha256> {
		compute_shallow(repo, wants, &deepen, client_shallow)
			.await
			.unwrap()
	}

	#[tokio::test]
	async fn depth_bounds_the_boundary() {
		let repo = new_repo().await;
		let a = commit(&repo, &[], 1).await; // root
		let b = commit(&repo, &[a], 2).await;
		let c = commit(&repo, &[b], 3).await; // tip

		// depth 1: only the tip; its parent is withheld, so the tip is the boundary.
		let p = plan(&repo, &[c], depth(1), &[]).await;
		assert_eq!(p.included, set(&[c]));
		assert_eq!(p.boundary, set(&[c]));
		assert_eq!(p.shallow, vec![c]);
		assert!(p.unshallow.is_empty());

		// depth 2: tip + parent; the parent is the boundary.
		let p = plan(&repo, &[c], depth(2), &[]).await;
		assert_eq!(p.included, set(&[c, b]));
		assert_eq!(p.boundary, set(&[b]));

		// depth beyond the history: the whole graph, and a root is never shallow.
		let p = plan(&repo, &[c], depth(10), &[]).await;
		assert_eq!(p.included, set(&[a, b, c]));
		assert!(p.boundary.is_empty());
		assert!(p.shallow.is_empty());
	}

	#[tokio::test]
	async fn since_bounds_by_committer_time() {
		let repo = new_repo().await;
		let a = commit(&repo, &[], 100).await;
		let b = commit(&repo, &[a], 200).await;
		let c = commit(&repo, &[b], 300).await;

		// since 250: the tip is in, its parent (200 < 250) is out — tip is boundary.
		let p = plan(&repo, &[c], since(250), &[]).await;
		assert_eq!(p.included, set(&[c]));
		assert_eq!(p.boundary, set(&[c]));

		// since 150: tip + parent in, grandparent (100 < 150) out — parent is boundary.
		let p = plan(&repo, &[c], since(150), &[]).await;
		assert_eq!(p.included, set(&[c, b]));
		assert_eq!(p.boundary, set(&[b]));
	}

	#[tokio::test]
	async fn deepen_not_excludes_the_ancestor_closure() {
		let repo = new_repo().await;
		let a = commit(&repo, &[], 1).await;
		let b = commit(&repo, &[a], 2).await;
		let c = commit(&repo, &[b], 3).await;
		repo
			.refs()
			.update_ref("refs/tags/mark", b, None)
			.await
			.unwrap();

		// deepen-not mark (= b) excludes b and a; walking from the tip, its parent b is excluded, so the
		// tip is the boundary.
		let p = plan(
			&repo,
			&[c],
			Deepen {
				not: vec!["refs/tags/mark".to_owned()],
				..Default::default()
			},
			&[],
		)
		.await;
		assert_eq!(p.included, set(&[c]));
		assert_eq!(p.boundary, set(&[c]));
	}

	#[tokio::test]
	async fn deepening_a_client_boundary_unshallows_it() {
		let repo = new_repo().await;
		let a = commit(&repo, &[], 1).await;
		let b = commit(&repo, &[a], 2).await;
		let c = commit(&repo, &[b], 3).await;

		// The client is shallow at the tip; deepening to depth 2 now sends the tip's parent, so the tip
		// unshallows and the parent becomes the new boundary.
		let p = plan(&repo, &[c], depth(2), &[c]).await;
		assert_eq!(p.boundary, set(&[b]));
		assert_eq!(p.shallow, vec![b]);
		assert_eq!(p.unshallow, vec![c]);
		// The pack walk is seeded with c's now-included parent b (the want c is a client have).
		assert_eq!(p.send_roots, vec![b]);
	}

	#[tokio::test]
	async fn relative_deepen_extends_below_the_client_frontier() {
		let repo = new_repo().await;
		let a = commit(&repo, &[], 1).await;
		let b = commit(&repo, &[a], 2).await;
		let c = commit(&repo, &[b], 3).await;

		// Client shallow at the tip; `--deepen 1` (relative) sends exactly one more level (b).
		let relative = |n| Deepen {
			depth: Some(n),
			relative: true,
			..Default::default()
		};
		let p = plan(&repo, &[c], relative(1), &[c]).await;
		assert_eq!(p.included, set(&[c, b]));
		assert_eq!(p.boundary, set(&[b]));
		assert_eq!(p.unshallow, vec![c]);
		assert_eq!(p.shallow, vec![b]);
		assert_eq!(p.send_roots, vec![b]);

		// `--deepen 2` reaches the root, which is never shallow, so the boundary empties out.
		let p = plan(&repo, &[c], relative(2), &[c]).await;
		assert_eq!(p.included, set(&[a, b, c]));
		assert!(p.boundary.is_empty());
		assert_eq!(p.unshallow, vec![c]);
	}

	#[tokio::test]
	async fn unshallow_completes_every_client_boundary_even_off_the_wants() {
		let repo = new_repo().await;
		// Two disjoint branches, each truncated at depth 1 on the client.
		let a = commit(&repo, &[], 1).await;
		let b = commit(&repo, &[a], 2).await; // "main" tip
		let x = commit(&repo, &[], 3).await;
		let y = commit(&repo, &[x], 4).await; // "other" tip
		let client_shallow = [b, y];

		// `--unshallow` wanting only `main` (b) still completes the unselected `other` (y) from the
		// client's `shallow` lines — matching git.
		let p = plan(&repo, &[b], unshallow(), &client_shallow).await;
		assert_eq!(p.included, set(&[a, b, x, y]));
		assert!(
			p.boundary.is_empty(),
			"no boundary remains after --unshallow"
		);
		assert!(p.shallow.is_empty());
		assert_eq!(
			set(&p.unshallow),
			set(&[b, y]),
			"both client boundaries unshallow"
		);
		// The pack is seeded with the now-exposed parents of both completed branches.
		assert_eq!(set(&p.send_roots), set(&[a, x]));

		// Contrast: a *finite* `--depth 1` re-depths only the wanted branch and leaves the unselected
		// branch's boundary untouched (git does the same), so `other` (y) is neither included nor
		// unshallowed.
		let p = plan(&repo, &[b], depth(1), &client_shallow).await;
		assert!(
			!p.included.contains(&y),
			"a finite depth does not touch the unselected branch"
		);
		assert!(p.unshallow.is_empty());
	}

	#[tokio::test]
	async fn rejects_unresolved_deepen_not() {
		let repo = new_repo().await;
		let c = commit(&repo, &[], 1).await;
		let deepen = Deepen {
			not: vec!["refs/tags/does-not-exist".to_owned()],
			..Default::default()
		};
		assert!(compute_shallow(&repo, &[c], &deepen, &[]).await.is_err());
	}

	fn depth(n: u32) -> Deepen {
		Deepen {
			depth: Some(n),
			..Default::default()
		}
	}

	fn since(t: i64) -> Deepen {
		Deepen {
			since: Some(t),
			..Default::default()
		}
	}

	/// `fetch --unshallow`: the unbounded absolute deepen sentinel.
	fn unshallow() -> Deepen {
		Deepen {
			depth: Some(INFINITE_DEPTH),
			..Default::default()
		}
	}
}
