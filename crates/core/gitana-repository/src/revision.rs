//! Revision resolution (`rev-parse`) and history walk (`rev-list`).
//!
//! Resolves the everyday `git rev-parse` syntax — full/abbreviated oid, ref names
//! (with a search order), `HEAD`/`@`, `~n`, `^n`, `^{type}` — and walks commits in
//! committer-date order. Abbreviated-oid resolution spans loose and packed objects.

use std::collections::{BinaryHeap, HashSet};

use gitana_file_store::FileStore;
use gitana_object::{
	Commit, HashAlgorithm, ObjectId, ObjectKind, Signature, parse_commit, parse_tag, parse_tree,
};

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
	spec: &str,
) -> Result<ObjectId<H>, RepositoryError> {
	// `<rev>:<path>` — the blob or tree at `<path>` within `<rev>`'s tree. (The `:path`/`:n:path`
	// index forms, which start with `:`, are not supported.)
	if let Some(colon) = spec.find(':')
		&& colon > 0
	{
		let base = resolve_rev(repo, &spec[..colon]).await?;
		return object_at_path(repo, base, &spec[colon + 1..]).await;
	}
	resolve_rev(repo, spec).await
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
	path: &str,
) -> Result<ObjectId<H>, RepositoryError> {
	let mut tree = peel_to_tree(repo, base).await?;
	// A trailing slash (`<rev>:<path>/`) requires the path to name a directory.
	let require_dir = path.ends_with('/');
	let path = path.trim_end_matches('/');
	if path.is_empty() {
		return Ok(tree);
	}
	let parts: Vec<&str> = path.split('/').collect();
	for (depth, part) in parts.iter().enumerate() {
		let (kind, payload) = repo.objects().read_object(&tree).await?;
		if kind != ObjectKind::Tree {
			return Err(RepositoryError::InvalidRef(format!(
				"path '{path}': '{}' is not a directory",
				parts[..depth].join("/")
			)));
		}
		let entry = parse_tree::<H>(&payload)?
			.into_iter()
			.find(|entry| entry.name == *part)
			.ok_or_else(|| {
				RepositoryError::InvalidRef(format!("path '{path}' does not exist in {base}"))
			})?;
		if depth + 1 == parts.len() {
			if require_dir && entry.mode != "40000" {
				return Err(RepositoryError::InvalidRef(format!(
					"path '{path}/' is not a directory"
				)));
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
