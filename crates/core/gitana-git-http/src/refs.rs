//! Gathering the refs a repository advertises, shared by the v0 advertisement and
//! the v2 `ls-refs` command.

use gitana_file_store::FileStore;
use gitana_object::{ObjectId, ObjectKind, parse_tag};
use gitana_repository::Repository;

use crate::GitHttpError;

/// The all-zero object id git uses for "no value" (e.g. an empty repo's tip).
pub(crate) const ZERO_OID: &str =
	"0000000000000000000000000000000000000000000000000000000000000000";

/// One advertised ref.
pub(crate) struct RefLine {
	/// The full ref name (`HEAD`, `refs/heads/main`, …).
	pub name: String,
	/// The object the ref points at.
	pub oid: ObjectId,
	/// For a symbolic ref (HEAD), the ref it points at.
	pub symref_target: Option<String>,
	/// For an annotated tag, the non-tag object it ultimately points at.
	pub peeled: Option<ObjectId>,
}

/// Collect advertised refs in wire order: `HEAD` first (when the repo has commits),
/// then refs under `refs/` sorted by name. With `peel`, annotated tags carry their
/// peeled target.
pub(crate) async fn collect_refs<F: FileStore>(
	repo: &Repository<F>,
	peel: bool,
) -> Result<Vec<RefLine>, GitHttpError> {
	let refs = repo.refs();
	let mut out = Vec::new();

	let head_target = refs.read_symbolic("HEAD").await?;
	if let Some(oid) = refs.resolve_head().await? {
		out.push(RefLine {
			name: "HEAD".to_owned(),
			oid,
			symref_target: head_target,
			peeled: None,
		});
	}

	for (name, oid) in refs.list("refs/").await? {
		let peeled = if peel {
			peel_tag(repo, oid).await
		} else {
			None
		};
		out.push(RefLine {
			name,
			oid,
			symref_target: None,
			peeled,
		});
	}
	Ok(out)
}

/// Follow an annotated-tag chain to the first non-tag object, or `None` if `oid` is
/// not a tag. Best-effort: a missing or unreadable object yields no peel rather than
/// an error (the ref itself is still advertised).
async fn peel_tag<F: FileStore>(repo: &Repository<F>, oid: ObjectId) -> Option<ObjectId> {
	let mut current = oid;
	let mut peeled = None;
	loop {
		let (kind, data) = repo.objects().read_object(&current).await.ok()?;
		if kind != ObjectKind::Tag {
			return peeled;
		}
		let tag = parse_tag(&data).ok()?;
		current = tag.object;
		peeled = Some(current);
	}
}
