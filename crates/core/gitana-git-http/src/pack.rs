//! Building the packfile to send for a fetch: the objects reachable from the
//! `want`s but not from the `have`s the client already has.
//!
//! The async counterpart to [`gitana_object::enumerate_objects`] (which takes a sync
//! reader): it walks the object graph reading from the async object store, reusing
//! [`gitana_object::referenced_ids`] to find each object's children.

use std::collections::HashSet;

use gitana_file_store::FileStore;
use gitana_object::{
	HashAlgorithm, ObjectId, ObjectKind, PackedObject, encode_pack, encode_pack_with_bases,
	parse_commit, referenced_ids,
};
use gitana_object_store::ObjectStoreError;
use gitana_repository::Repository;

use crate::GitHttpError;

/// Cap on the total payload of boundary objects retained as thin-pack delta bases. The
/// have-closure can be the whole repository, but only objects near the tips make useful
/// bases for an incremental change; a DFS from the have tips reaches those first, so
/// this budget keeps recent boundary objects and drops the deep tail. Correctness is
/// unaffected — dropping a base just forgoes a delta (the object is sent full).
///
/// Follow-up: git bounds thin bases to the *edge* objects adjacent to the cut rather
/// than a byte budget over the closure; that is a tighter, more targeted heuristic.
const MAX_THIN_BASE_BYTES: usize = 64 * 1024 * 1024;

/// Build a delta-compressed packfile carrying the objects reachable from `wants` but
/// not from `haves`. An empty want set yields an empty (header + trailer) pack.
///
/// Used server-side to answer a fetch, and client-side (`gta push`) to pack the
/// objects a push must send. The pack is **self-contained** (every delta base is
/// carried in it); see [`build_pack_thin`] for the thin counterpart.
pub async fn build_pack<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
) -> Result<Vec<u8>, GitHttpError> {
	let (objects, _bases) =
		collect_objects(repo, wants, haves, false, &HashSet::new(), &HashSet::new()).await?;
	Ok(encode_pack(&objects))
}

/// Like [`build_pack`], but for a shallow fetch ([`crate::shallow`]): the commit walk stops at the
/// send-side `boundary` (a boundary commit's tree is sent but its parents are not), and the have-side
/// walk stops at `have_boundary` — the commits the client itself is shallow at, whose parents it does
/// *not* have, so they must not be subtracted from the pack (letting `--unshallow`/deepen send them).
/// Empty sets reduce to a normal complete pack.
pub async fn build_pack_shallow<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
	boundary: &HashSet<ObjectId<H>>,
	have_boundary: &HashSet<ObjectId<H>>,
) -> Result<Vec<u8>, GitHttpError> {
	let (objects, _bases) =
		collect_objects(repo, wants, haves, false, boundary, have_boundary).await?;
	Ok(encode_pack(&objects))
}

/// Build a **thin** packfile for the same object set as [`build_pack`], additionally
/// deltifying against the boundary objects the peer already has (the have-closure) as
/// external bases referenced by id and never carried in the pack. The peer must
/// complete the pack against its object store before storing it.
///
/// Used when the peer negotiated `thin-pack` (fetch) or on the push send path (stock
/// git and gitana both complete an incoming thin pack).
pub async fn build_pack_thin<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
) -> Result<Vec<u8>, GitHttpError> {
	let (objects, bases) =
		collect_objects(repo, wants, haves, true, &HashSet::new(), &HashSet::new()).await?;
	Ok(encode_pack_with_bases(&objects, &bases))
}

/// Collect the objects to send: reachable-from-`wants` minus reachable-from-`haves`.
/// When `thin`, also return the boundary objects (the have-closure, up to
/// [`MAX_THIN_BASE_BYTES`]) to offer as external delta bases. A commit in `shallow_boundary` has its
/// tree sent but its parents withheld — the walk stops there (a shallow fetch); an empty set is a
/// normal complete walk.
async fn collect_objects<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
	thin: bool,
	shallow_boundary: &HashSet<ObjectId<H>>,
	have_boundary: &HashSet<ObjectId<H>>,
) -> Result<(Vec<PackedObject<H>>, Vec<PackedObject<H>>), GitHttpError> {
	let store = repo.objects();

	// Pass 1: mark everything reachable from the haves we actually have. An unknown
	// have is ignored (the client may report objects the server lacks). When thin,
	// retain the objects read here (near-tip first) as candidate delta bases, capped.
	let mut excluded: HashSet<ObjectId<H>> = HashSet::new();
	let mut bases: Vec<PackedObject<H>> = Vec::new();
	let mut bases_bytes = 0usize;
	let mut stack: Vec<ObjectId<H>> = haves.to_vec();
	while let Some(id) = stack.pop() {
		if !excluded.insert(id) {
			continue;
		}
		match store.read_object(&id).await {
			Ok((kind, data)) => {
				if kind == ObjectKind::Commit && have_boundary.contains(&id) {
					// The client is shallow at this commit: it has the commit and its tree but NOT its
					// parents, so the have-walk must stop here — otherwise we would subtract ancestors the
					// client lacks and omit them from an `--unshallow`/deepen pack.
					stack.push(parse_commit::<H>(&data)?.tree);
				} else {
					stack.extend(referenced_ids::<H>(kind, &data)?);
				}
				// Retain as a base only if it fits the remaining budget, so no single large
				// object pushes the retained set past the cap (an oversize base is skipped).
				if thin && bases_bytes.saturating_add(data.len()) <= MAX_THIN_BASE_BYTES {
					bases_bytes += data.len();
					bases.push(PackedObject { id, kind, data });
				}
			}
			Err(ObjectStoreError::NotFound) => {}
			Err(other) => return Err(other.into()),
		}
	}

	// Pass 2: collect reachable-from-wants minus the excluded set. An object reached
	// from a want must exist (connectivity); a missing one is a real error.
	let mut collected: HashSet<ObjectId<H>> = HashSet::new();
	let mut result: Vec<PackedObject<H>> = Vec::new();
	let mut stack: Vec<ObjectId<H>> = wants.to_vec();
	while let Some(id) = stack.pop() {
		if excluded.contains(&id) || !collected.insert(id) {
			continue;
		}
		let (kind, data) = store.read_object(&id).await?;
		if kind == ObjectKind::Commit {
			// A commit's tree is always sent; its parents are followed unless this is a shallow-boundary
			// commit, whose parents are deliberately withheld so the walk stops here.
			let commit = parse_commit::<H>(&data)?;
			stack.push(commit.tree);
			if !shallow_boundary.contains(&id) {
				stack.extend(commit.parents.iter().copied());
			}
		} else {
			stack.extend(referenced_ids::<H>(kind, &data)?);
		}
		result.push(PackedObject { id, kind, data });
	}
	Ok((result, bases))
}
