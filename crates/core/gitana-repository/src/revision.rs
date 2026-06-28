//! Revision resolution (`rev-parse`) and history walk (`rev-list`).
//!
//! Resolves the everyday `git rev-parse` syntax — full/abbreviated oid, ref names
//! (with a search order), `HEAD`/`@`, `~n`, `^n`, `^{type}` — and walks commits in
//! committer-date order. Abbreviated-oid resolution scans loose objects only;
//! packed-object abbreviation is a follow-up.

use std::collections::{BinaryHeap, HashSet};

use gitana_file_store::FileStore;
use gitana_object::{Commit, ObjectId, ObjectKind, Signature, parse_commit, parse_tag};

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
pub(crate) async fn rev_parse(
	repo: &Repository<impl FileStore>,
	spec: &str,
) -> Result<ObjectId, RepositoryError> {
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

/// Walk commits reachable from `tips` in committer-date order (newest first).
pub(crate) async fn rev_list(
	repo: &Repository<impl FileStore>,
	tips: &[ObjectId],
) -> Result<Vec<ObjectId>, RepositoryError> {
	let mut heap: BinaryHeap<(i64, ObjectId)> = BinaryHeap::new();
	let mut seen: HashSet<ObjectId> = HashSet::new();

	for tip in tips {
		let commit = peel_to_commit(repo, *tip).await?;
		if seen.insert(commit) {
			heap.push((committer_seconds(repo, commit).await?, commit));
		}
	}

	let mut out = Vec::new();
	while let Some((_, id)) = heap.pop() {
		out.push(id);
		for parent in read_commit(repo, id).await?.parents {
			if seen.insert(parent) {
				heap.push((committer_seconds(repo, parent).await?, parent));
			}
		}
	}
	Ok(out)
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

async fn resolve_base(
	repo: &Repository<impl FileStore>,
	base: &str,
) -> Result<ObjectId, RepositoryError> {
	if base == "HEAD" || base == "@" {
		return repo
			.refs()
			.resolve_head()
			.await?
			.ok_or_else(|| RepositoryError::InvalidRef("HEAD (unborn branch)".to_owned()));
	}

	// Ref search order, then oid.
	for candidate in [
		base.to_owned(),
		format!("refs/{base}"),
		format!("refs/tags/{base}"),
		format!("refs/heads/{base}"),
	] {
		if let Some(id) = repo.refs().resolve(&candidate).await? {
			return Ok(id);
		}
	}

	if is_hex(base) && (4..=64).contains(&base.len()) {
		if base.len() == 64 {
			let id =
				ObjectId::from_hex(base).map_err(|_| RepositoryError::InvalidRef(base.to_owned()))?;
			if repo.objects().exists_object(&id).await? {
				return Ok(id);
			}
		} else {
			return resolve_abbrev(repo, base).await;
		}
	}
	Err(RepositoryError::InvalidRef(format!(
		"unknown revision: {base}"
	)))
}

async fn resolve_abbrev(
	repo: &Repository<impl FileStore>,
	hex: &str,
) -> Result<ObjectId, RepositoryError> {
	let (dir, rest) = hex.split_at(2);
	let entries = repo
		.objects()
		.file_store()
		.list_prefix(&format!("objects/{dir}/"))
		.await?;

	let mut matches: Vec<ObjectId> = Vec::new();
	for path in entries {
		let name = path.rsplit('/').next().unwrap_or_default();
		if name.starts_with(rest)
			&& let Ok(id) = ObjectId::from_hex(&format!("{dir}{name}"))
		{
			matches.push(id);
		}
	}
	match matches.as_slice() {
		[only] => Ok(*only),
		[] => Err(RepositoryError::InvalidRef(format!(
			"unknown revision: {hex}"
		))),
		_ => Err(RepositoryError::InvalidRef(format!(
			"ambiguous abbreviation: {hex}"
		))),
	}
}

async fn read_commit(
	repo: &Repository<impl FileStore>,
	id: ObjectId,
) -> Result<Commit, RepositoryError> {
	let (kind, payload) = repo.objects().read_object(&id).await?;
	if kind != ObjectKind::Commit {
		return Err(RepositoryError::InvalidRef(format!("{id} is not a commit")));
	}
	Ok(parse_commit(&payload)?)
}

async fn peel_to_commit(
	repo: &Repository<impl FileStore>,
	mut id: ObjectId,
) -> Result<ObjectId, RepositoryError> {
	loop {
		let (kind, payload) = repo.objects().read_object(&id).await?;
		match kind {
			ObjectKind::Commit => return Ok(id),
			ObjectKind::Tag => id = parse_tag(&payload)?.object,
			_ => return Err(RepositoryError::InvalidRef(format!("{id} is not a commit"))),
		}
	}
}

async fn nth_parent(
	repo: &Repository<impl FileStore>,
	id: ObjectId,
	n: u32,
) -> Result<ObjectId, RepositoryError> {
	let commit_id = peel_to_commit(repo, id).await?;
	if n == 0 {
		return Ok(commit_id);
	}
	read_commit(repo, commit_id)
		.await?
		.parents
		.get((n - 1) as usize)
		.copied()
		.ok_or_else(|| RepositoryError::InvalidRef(format!("{commit_id} has no parent {n}")))
}

async fn first_parent_n(
	repo: &Repository<impl FileStore>,
	id: ObjectId,
	n: u32,
) -> Result<ObjectId, RepositoryError> {
	let mut current = peel_to_commit(repo, id).await?;
	for _ in 0..n {
		current = *read_commit(repo, current)
			.await?
			.parents
			.first()
			.ok_or_else(|| RepositoryError::InvalidRef(format!("{current} has no parent")))?;
	}
	Ok(current)
}

async fn peel_type(
	repo: &Repository<impl FileStore>,
	id: ObjectId,
	kind: &str,
) -> Result<ObjectId, RepositoryError> {
	match kind {
		"" => {
			// Deref tags to a non-tag object.
			let mut current = id;
			loop {
				let (object_kind, payload) = repo.objects().read_object(&current).await?;
				if object_kind == ObjectKind::Tag {
					current = parse_tag(&payload)?.object;
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

async fn committer_seconds(
	repo: &Repository<impl FileStore>,
	id: ObjectId,
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
