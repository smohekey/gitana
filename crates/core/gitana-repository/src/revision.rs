//! Revision resolution (`rev-parse`) and history walk (`rev-list`).
//!
//! Resolves the everyday `git rev-parse` syntax — full/abbreviated oid, ref names
//! (with a search order), `HEAD`/`@`, `~n`, `^n`, `^{type}` — and walks commits in
//! committer-date order. Abbreviated-oid resolution spans loose and packed objects.

use std::collections::{BinaryHeap, HashMap, HashSet};

use gitana_file_store::FileStore;
use gitana_object::{
	Commit, HashAlgorithm, ObjectId, ObjectKind, Signature, parse_commit, parse_tag, parse_tree,
};
use gitana_path::{GitPath, GitPathspec};

use crate::{Repository, RepositoryError};

enum Op {
	/// `^n` — the nth parent (`^0` peels to the commit itself).
	Parent(u32),
	/// `~n` — the nth first-parent ancestor.
	Ancestor(u32),
	/// `^{type}` — peel to a type (`""` derefs tags to a non-tag).
	Peel(String),
}

/// Resolve a revision spec to an object id.
pub(crate) async fn rev_parse<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	spec: &[u8],
) -> Result<ObjectId<H>, RepositoryError> {
	if spec.contains(&0) {
		return Err(RepositoryError::InvalidRef(
			"revision specifications cannot contain NUL bytes".to_owned(),
		));
	}
	// `<rev>:<path>` — the blob or tree at `<path>` within `<rev>`'s tree. (The `:path`/`:n:path`
	// index forms, which start with `:`, are not supported.)
	if let Some(colon) = spec.iter().position(|byte| *byte == b':')
		&& colon > 0
	{
		let base = revision_text(&spec[..colon])?;
		let base = resolve_rev(repo, base).await?;
		return object_at_path(repo, base, &spec[colon + 1..]).await;
	}
	resolve_rev(repo, revision_text(spec)?).await
}

fn revision_text(spec: &[u8]) -> Result<&str, RepositoryError> {
	std::str::from_utf8(spec).map_err(|_| {
		RepositoryError::InvalidRevisionEncoding(
			GitPathspec::from_bytes(spec.to_vec()).expect("rev_parse rejects NUL bytes"),
		)
	})
}

/// Resolve a revision spec without a `:path` suffix: a base (ref/oid/`HEAD`) and `~`/`^` ops.
async fn resolve_rev<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	spec: &str,
) -> Result<ObjectId<H>, RepositoryError> {
	let (base, ops) = parse_spec(spec)?;
	let mut id = resolve_base(repo, base).await?;
	for op in ops {
		id = match op {
			Op::Parent(n) => nth_parent(repo, id, n).await?,
			Op::Ancestor(n) => first_parent_n(repo, id, n).await?,
			Op::Peel(kind) => peel_type(repo, id, &kind).await?,
		};
	}
	Ok(id)
}

/// Resolve `<path>` within the tree of `base` (a commit, tag, or tree) to its blob or tree id. An
/// empty path yields the tree itself; descending through a non-directory is an error.
async fn object_at_path<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	base: ObjectId<H>,
	path: &[u8],
) -> Result<ObjectId<H>, RepositoryError> {
	let mut tree = peel_to_tree(repo, base).await?;
	let supplied_path = GitPathspec::from_bytes(path.to_vec()).expect("rev_parse rejects NUL bytes");
	// A trailing slash (`<rev>:<path>/`) requires the path to name a directory.
	let require_dir = path.ends_with(b"/");
	let mut path = path;
	while let Some(trimmed) = path.strip_suffix(b"/") {
		path = trimmed;
	}
	let path =
		GitPath::from_bytes(path.to_vec()).map_err(|reason| RepositoryError::InvalidTreePath {
			path: supplied_path.clone(),
			reason,
		})?;
	if path.is_root() {
		return Ok(tree);
	}
	let parts: Vec<&[u8]> = path.components().collect();
	for (depth, part) in parts.iter().enumerate() {
		let (kind, payload) = repo.objects().read_object(&tree).await?;
		if kind != ObjectKind::Tree {
			let mut parent = Vec::new();
			for (index, component) in parts[..depth].iter().enumerate() {
				if index > 0 {
					parent.push(b'/');
				}
				parent.extend_from_slice(component);
			}
			let parent = GitPath::from_bytes(parent).expect("tree path components remain canonical");
			return Err(RepositoryError::TreePathComponentNotDirectory {
				path,
				component: parent,
			});
		}
		let entry = parse_tree::<H>(&payload)?
			.into_iter()
			.find(|entry| entry.name.as_bytes() == *part)
			.ok_or_else(|| RepositoryError::MissingTreePath {
				path: path.clone(),
				base: base.to_string(),
			})?;
		if depth + 1 == parts.len() {
			if require_dir && entry.mode != "40000" {
				return Err(RepositoryError::TreePathNotDirectory(supplied_path));
			}
			return Ok(entry.id);
		}
		tree = entry.id;
	}
	unreachable!("the last component returns")
}

/// Peel a commit/tag/tree id to a tree id (dereferencing tags), erroring on a blob.
pub(crate) async fn peel_to_tree<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	mut id: ObjectId<H>,
) -> Result<ObjectId<H>, RepositoryError> {
	loop {
		let (kind, payload) = repo.objects().read_object(&id).await?;
		match kind {
			ObjectKind::Tree => return Ok(id),
			ObjectKind::Commit => return Ok(parse_commit::<H>(&payload)?.tree),
			ObjectKind::Tag => id = parse_tag::<H>(&payload)?.object,
			ObjectKind::Blob => {
				return Err(RepositoryError::InvalidRef(format!(
					"{id} is not a tree-ish"
				)));
			}
		}
	}
}

/// Walk commits reachable from `tips` in committer-date order (newest first). A commit at the
/// repository's shallow boundary (`.git/shallow`) is treated as parentless, so the walk stops there
/// instead of following an edge to a deliberately-absent parent object.
pub(crate) async fn rev_list<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	tips: &[ObjectId<H>],
) -> Result<Vec<ObjectId<H>>, RepositoryError> {
	let shallow = shallow_set(repo).await?;
	let mut heap: BinaryHeap<(i64, ObjectId<H>)> = BinaryHeap::new();
	let mut seen: HashSet<ObjectId<H>> = HashSet::new();

	for tip in tips {
		let commit = peel_to_commit(repo, *tip).await?;
		if seen.insert(commit) {
			heap.push((committer_seconds(repo, commit).await?, commit));
		}
	}

	let mut out = Vec::new();
	while let Some((_, id)) = heap.pop() {
		out.push(id);
		for parent in commit_parents(repo, id, &shallow).await? {
			if seen.insert(parent) {
				heap.push((committer_seconds(repo, parent).await?, parent));
			}
		}
	}
	Ok(out)
}

/// How many more commits a bounded walk ([`rev_list_excluding`], [`reachable_among`]) takes once it
/// could stop, git's `SLOP`: a margin for commit dates that run backwards (a child dated before its
/// parent).
const SLOP: usize = 5;

/// A commit's committer date and parents (none at the shallow boundary), read once.
async fn date_and_parents<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	id: ObjectId<H>,
	shallow: &HashSet<ObjectId<H>>,
) -> Result<(i64, Vec<ObjectId<H>>), RepositoryError> {
	let commit = read_commit(repo, id).await?;
	let date = Signature::parse(&commit.committer)?.seconds;
	let parents = if shallow.contains(&id) {
		Vec::new()
	} else {
		commit.parents
	};
	Ok((date, parents))
}

/// The commits `include` reaches that `exclude` doesn't — git's `rev-list <include> ^<exclude>` —
/// newest first by committer date, as [`rev_list`] orders them. Unlike [`rev_list`] it doesn't drain
/// history: it walks both sides in date order and stops as git's `limit_list` does, once every queued
/// commit is excluded and older than the oldest one listed, plus [`SLOP`] more, so its cost is the
/// history between them, not all of it. As in git, an excluded commit's exclusion passes at once
/// through every commit already read (queued or walked) to their parents. Exact while commit dates
/// don't run backwards further than that margin, as git's is. The shallow boundary is honoured as
/// [`rev_list`] honours it.
pub(crate) async fn rev_list_excluding<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	include: &[ObjectId<H>],
	exclude: &[ObjectId<H>],
) -> Result<Vec<ObjectId<H>>, RepositoryError> {
	let shallow = shallow_set(repo).await?;
	let mut heap: BinaryHeap<(i64, ObjectId<H>)> = BinaryHeap::new();
	// The parents of every commit read so far, so an exclusion passes through them.
	let mut parents_of: HashMap<ObjectId<H>, Vec<ObjectId<H>>> = HashMap::new();
	let mut pending: HashSet<ObjectId<H>> = HashSet::new();
	let mut hidden: HashSet<ObjectId<H>> = HashSet::new();
	// Queued commits not (yet) known to be excluded: while there are any, the walk goes on.
	let mut visible: usize = 0;

	let mut tips = Vec::new();
	for tip in exclude {
		let commit = peel_to_commit(repo, *tip).await?;
		hidden.insert(commit);
		tips.push(commit);
	}
	for tip in include {
		tips.push(peel_to_commit(repo, *tip).await?);
	}
	for commit in tips {
		if let std::collections::hash_map::Entry::Vacant(entry) = parents_of.entry(commit) {
			let (date, parents) = date_and_parents(repo, commit, &shallow).await?;
			entry.insert(parents);
			pending.insert(commit);
			if !hidden.contains(&commit) {
				visible += 1;
			}
			heap.push((date, commit));
		}
	}
	// The excluded tips' own parents are excluded too, as soon as they're known.
	let excluded: Vec<ObjectId<H>> = hidden.iter().copied().collect();
	for commit in excluded {
		hide_beyond(commit, &parents_of, &mut hidden, &pending, &mut visible);
	}

	let mut out = Vec::new();
	// The date of the oldest commit listed so far: an excluded commit queued at or after it may
	// still reach one of them.
	let mut oldest_listed: Option<i64> = None;
	let mut slop = SLOP;
	while let Some((date, id)) = heap.pop() {
		pending.remove(&id);
		let was_hidden = hidden.contains(&id);
		if !was_hidden {
			visible -= 1;
		}
		let parents = parents_of.get(&id).cloned().unwrap_or_default();
		for parent in parents {
			if let std::collections::hash_map::Entry::Vacant(entry) = parents_of.entry(parent) {
				let (parent_date, grandparents) = date_and_parents(repo, parent, &shallow).await?;
				entry.insert(grandparents);
				pending.insert(parent);
				if !hidden.contains(&parent) {
					visible += 1;
				}
				heap.push((parent_date, parent));
				// Reached from an excluded commit: excluded, and what it leads to as far as known.
				if hidden.contains(&parent) {
					hide_beyond(parent, &parents_of, &mut hidden, &pending, &mut visible);
				}
			}
		}
		if was_hidden {
			hide_beyond(id, &parents_of, &mut hidden, &pending, &mut visible);
			let newest_queued = heap.peek().map(|(date, _)| *date);
			let may_reach_listed = matches!(
				(oldest_listed, newest_queued),
				(Some(listed), Some(queued)) if listed <= queued
			);
			if visible > 0 || may_reach_listed {
				slop = SLOP;
				continue;
			}
			slop -= 1;
			if slop == 0 {
				break;
			}
			continue;
		}
		oldest_listed = Some(date);
		out.push(id);
	}
	out.retain(|id| !hidden.contains(id));
	Ok(out)
}

/// Excludes everything `commit` (excluded) leads to, as far as the commits read so far tell
/// (`parents_of`), keeping `visible` the count of queued commits not excluded.
fn hide_beyond<H: HashAlgorithm>(
	commit: ObjectId<H>,
	parents_of: &HashMap<ObjectId<H>, Vec<ObjectId<H>>>,
	hidden: &mut HashSet<ObjectId<H>>,
	pending: &HashSet<ObjectId<H>>,
	visible: &mut usize,
) {
	let mut stack: Vec<ObjectId<H>> = parents_of.get(&commit).cloned().unwrap_or_default();
	while let Some(next) = stack.pop() {
		if hidden.insert(next) {
			if pending.contains(&next) {
				*visible -= 1;
			}
			if let Some(further) = parents_of.get(&next) {
				stack.extend(further.iter().copied());
			}
		}
	}
}

/// Which of `commits` the `tips` reach, and the first of `tips` (in the order given) that reaches
/// each: a map from each commit reached to that tip's index. One walk over all the tips at once,
/// newest first by committer date, each commit carrying the earliest tip that reaches it; it stops
/// once neither the commit just taken nor anything queued is as new as the oldest of them, plus
/// [`SLOP`] more commits taken, since an older commit can't lead to a newer one. Its cost is the history no older than `commits`, once, however many tips.
///
/// It's a bound, not proof: under skewed commit clocks a reachable commit can be missed, where the
/// path to it runs through commits dated before it and the margin, counted over every commit taken
/// (a merge's other parents' lines use it up too), runs out first; and the tip named can then be a
/// later one than the first. Exact where commit dates don't run backwards. git's exact
/// `--contains` uses generation numbers, which gitana doesn't have. The shallow boundary is
/// honoured as [`rev_list`] honours it.
pub(crate) async fn reachable_among<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	tips: &[ObjectId<H>],
	commits: &[ObjectId<H>],
) -> Result<HashMap<ObjectId<H>, usize>, RepositoryError> {
	let mut wanted: HashSet<ObjectId<H>> = HashSet::new();
	let mut oldest: Option<i64> = None;
	for commit in commits {
		let commit = peel_to_commit(repo, *commit).await?;
		if wanted.insert(commit) {
			let date = committer_seconds(repo, commit).await?;
			oldest = Some(oldest.map_or(date, |oldest: i64| oldest.min(date)));
		}
	}
	let Some(oldest) = oldest else {
		return Ok(HashMap::new());
	};
	let shallow = shallow_set(repo).await?;
	let mut heap: BinaryHeap<(i64, ObjectId<H>)> = BinaryHeap::new();
	// Every commit read, its parents, and the earliest tip found to reach it.
	let mut parents_of: HashMap<ObjectId<H>, Vec<ObjectId<H>>> = HashMap::new();
	let mut first: HashMap<ObjectId<H>, usize> = HashMap::new();
	for (index, tip) in tips.iter().enumerate() {
		let commit = peel_to_commit(repo, *tip).await?;
		if let std::collections::hash_map::Entry::Vacant(entry) = parents_of.entry(commit) {
			let (date, parents) = date_and_parents(repo, commit, &shallow).await?;
			entry.insert(parents);
			first.insert(commit, index);
			heap.push((date, commit));
		}
	}
	// Taking one of `commits` doesn't settle it: a commit of the same date taken after it can still
	// bring an earlier tip to it, so the walk goes on through everything no older than them.
	let mut slop = SLOP;
	while let Some((date, id)) = heap.pop() {
		let index = first[&id];
		for parent in parents_of.get(&id).cloned().unwrap_or_default() {
			if let std::collections::hash_map::Entry::Vacant(entry) = parents_of.entry(parent) {
				let (parent_date, grandparents) = date_and_parents(repo, parent, &shallow).await?;
				entry.insert(grandparents);
				first.insert(parent, index);
				heap.push((parent_date, parent));
			} else {
				// Reached before by a later tip: this one comes first, and so it does for what that
				// commit leads to, as far as read.
				let mut stack = vec![parent];
				while let Some(next) = stack.pop() {
					if first.get(&next).is_some_and(|known| *known > index) {
						first.insert(next, index);
						if let Some(further) = parents_of.get(&next) {
							stack.extend(further.iter().copied());
						}
					}
				}
			}
		}
		// Done only once neither the commit just taken nor anything queued (its parents included)
		// is as new as the oldest one asked about.
		if date >= oldest || heap.peek().is_some_and(|(queued, _)| *queued >= oldest) {
			slop = SLOP;
			continue;
		}
		slop -= 1;
		if slop == 0 {
			break;
		}
	}
	Ok(
		first
			.into_iter()
			.filter(|(id, _)| wanted.contains(id))
			.collect(),
	)
}

/// The commit's parents, honoring the shallow boundary: a commit in `shallow` is treated as having no
/// parents (its parent objects are deliberately absent), so history walks stop at it rather than
/// failing to read a missing parent. Shared by `rev-list` and the merge-base ancestry walks.
pub(crate) async fn commit_parents<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	id: ObjectId<H>,
	shallow: &HashSet<ObjectId<H>>,
) -> Result<Vec<ObjectId<H>>, RepositoryError> {
	if shallow.contains(&id) {
		return Ok(Vec::new());
	}
	Ok(read_commit(repo, id).await?.parents)
}

/// The repository's shallow boundary as a set, for the ancestry walks (empty for a complete repo).
pub(crate) async fn shallow_set<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<HashSet<ObjectId<H>>, RepositoryError> {
	Ok(repo.read_shallow().await?.into_iter().collect())
}

fn parse_spec(spec: &str) -> Result<(&str, Vec<Op>), RepositoryError> {
	let split = spec.find(['~', '^']).unwrap_or(spec.len());
	let base = &spec[..split];
	let mut rest = &spec[split..];
	let mut ops = Vec::new();

	while let Some(first) = rest.chars().next() {
		match first {
			'^' if rest[1..].starts_with('{') => {
				let end = rest
					.find('}')
					.ok_or_else(|| RepositoryError::InvalidRef(spec.to_owned()))?;
				ops.push(Op::Peel(rest[2..end].to_owned()));
				rest = &rest[end + 1..];
			}
			'^' => {
				let (n, len) = leading_number(&rest[1..]);
				ops.push(Op::Parent(n.unwrap_or(1)));
				rest = &rest[1 + len..];
			}
			'~' => {
				let (n, len) = leading_number(&rest[1..]);
				ops.push(Op::Ancestor(n.unwrap_or(1)));
				rest = &rest[1 + len..];
			}
			_ => return Err(RepositoryError::InvalidRef(spec.to_owned())),
		}
	}
	Ok((base, ops))
}

fn leading_number(s: &str) -> (Option<u32>, usize) {
	let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
	let len = digits.len();
	(digits.parse().ok(), len)
}

async fn resolve_base<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	base: &str,
) -> Result<ObjectId<H>, RepositoryError> {
	if base == "HEAD" || base == "@" {
		return repo
			.refs()
			.resolve_head()
			.await?
			.ok_or_else(|| RepositoryError::UnknownRevision("HEAD (unborn branch)".to_owned()));
	}

	// Ref search order, then oid. Mirrors git's gitrevisions(7) sequence for a bare name: verbatim,
	// refs/, refs/tags/, refs/heads/, refs/remotes/, and finally the remote's symbolic HEAD
	// (`origin` → refs/remotes/origin/HEAD). A remote's HEAD — and `refs/remotes/origin/HEAD` or
	// `origin/HEAD` naming it — is a symbolic `ref:` pointer, so follow symbolic targets throughout.
	for candidate in [
		base.to_owned(),
		format!("refs/{base}"),
		format!("refs/tags/{base}"),
		format!("refs/heads/{base}"),
		format!("refs/remotes/{base}"),
		format!("refs/remotes/{base}/HEAD"),
	] {
		if let Some(id) = repo.refs().resolve_symbolic(&candidate).await? {
			return Ok(id);
		}
	}

	let full_len = H::RAW_LEN * 2;
	if is_hex(base) && (4..=full_len).contains(&base.len()) {
		if base.len() == full_len {
			let id =
				ObjectId::from_hex(base).map_err(|_| RepositoryError::InvalidRef(base.to_owned()))?;
			if repo.objects().exists_object(&id).await? {
				return Ok(id);
			}
		} else {
			return resolve_abbrev(repo, base).await;
		}
	}
	Err(RepositoryError::UnknownRevision(base.to_owned()))
}

async fn resolve_abbrev<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	hex: &str,
) -> Result<ObjectId<H>, RepositoryError> {
	match repo.objects().find_by_prefix(hex).await?.as_slice() {
		[only] => Ok(*only),
		[] => Err(RepositoryError::UnknownRevision(hex.to_owned())),
		_ => Err(RepositoryError::AmbiguousRevision(hex.to_owned())),
	}
}

pub(crate) async fn read_commit<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	id: ObjectId<H>,
) -> Result<Commit<H>, RepositoryError> {
	let (kind, payload) = repo.objects().read_object(&id).await?;
	if kind != ObjectKind::Commit {
		return Err(RepositoryError::InvalidRef(format!("{id} is not a commit")));
	}
	Ok(parse_commit::<H>(&payload)?)
}

pub(crate) async fn peel_to_commit<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	mut id: ObjectId<H>,
) -> Result<ObjectId<H>, RepositoryError> {
	loop {
		let (kind, payload) = repo.objects().read_object(&id).await?;
		match kind {
			ObjectKind::Commit => return Ok(id),
			ObjectKind::Tag => id = parse_tag::<H>(&payload)?.object,
			_ => return Err(RepositoryError::InvalidRef(format!("{id} is not a commit"))),
		}
	}
}

/// Like [`peel_to_commit`], but returns `None` when `id` does not peel to a commit — a blob or tree,
/// e.g. a lightweight tag pointing straight at one — rather than erroring. For callers that must
/// classify a possibly-non-commit ref without aborting (fetch's fast-forward vs "storing tag"
/// wording).
pub(crate) async fn try_peel_to_commit<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	mut id: ObjectId<H>,
) -> Result<Option<ObjectId<H>>, RepositoryError> {
	loop {
		let (kind, payload) = repo.objects().read_object(&id).await?;
		match kind {
			ObjectKind::Commit => return Ok(Some(id)),
			ObjectKind::Tag => id = parse_tag::<H>(&payload)?.object,
			_ => return Ok(None),
		}
	}
}

async fn nth_parent<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	id: ObjectId<H>,
	n: u32,
) -> Result<ObjectId<H>, RepositoryError> {
	let commit_id = peel_to_commit(repo, id).await?;
	if n == 0 {
		return Ok(commit_id);
	}
	// A shallow-boundary commit is parentless, so `^n` past it fails (as in git) rather than returning
	// a deliberately-absent parent id.
	let shallow = shallow_set(repo).await?;
	commit_parents(repo, commit_id, &shallow)
		.await?
		.get((n - 1) as usize)
		.copied()
		.ok_or_else(|| RepositoryError::InvalidRef(format!("{commit_id} has no parent {n}")))
}

async fn first_parent_n<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	id: ObjectId<H>,
	n: u32,
) -> Result<ObjectId<H>, RepositoryError> {
	let shallow = shallow_set(repo).await?;
	let mut current = peel_to_commit(repo, id).await?;
	for _ in 0..n {
		// A `~n` that would cross a shallow boundary fails, matching git — the parent is absent.
		current = *commit_parents(repo, current, &shallow)
			.await?
			.first()
			.ok_or_else(|| RepositoryError::InvalidRef(format!("{current} has no parent")))?;
	}
	Ok(current)
}

async fn peel_type<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	id: ObjectId<H>,
	kind: &str,
) -> Result<ObjectId<H>, RepositoryError> {
	match kind {
		"" => {
			// Deref tags to a non-tag object.
			let mut current = id;
			loop {
				let (object_kind, payload) = repo.objects().read_object(&current).await?;
				if object_kind == ObjectKind::Tag {
					current = parse_tag::<H>(&payload)?.object;
				} else {
					return Ok(current);
				}
			}
		}
		"commit" => peel_to_commit(repo, id).await,
		"tree" => {
			let commit = peel_to_commit(repo, id).await?;
			Ok(read_commit(repo, commit).await?.tree)
		}
		"blob" | "tag" => {
			let (object_kind, _) = repo.objects().read_object(&id).await?;
			let wanted = if kind == "blob" {
				ObjectKind::Blob
			} else {
				ObjectKind::Tag
			};
			if object_kind == wanted {
				Ok(id)
			} else {
				Err(RepositoryError::InvalidRef(format!("{id} is not a {kind}")))
			}
		}
		other => Err(RepositoryError::InvalidRef(format!(
			"unknown peel: ^{{{other}}}"
		))),
	}
}

pub(crate) async fn committer_seconds<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	id: ObjectId<H>,
) -> Result<i64, RepositoryError> {
	let commit = read_commit(repo, id).await?;
	Ok(Signature::parse(&commit.committer)?.seconds)
}

fn is_hex(s: &str) -> bool {
	!s.is_empty()
		&& s
			.bytes()
			.all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
