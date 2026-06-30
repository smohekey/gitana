//! Building the packfile to send for a fetch: the objects reachable from the
//! `want`s but not from the `have`s the client already has.
//!
//! The async counterpart to [`gitana_object::enumerate_objects`] (which takes a sync
//! reader): it walks the object graph reading from the async object store, reusing
//! [`gitana_object::referenced_ids`] to find each object's children.

use std::collections::HashSet;

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId, PackedObject, encode_pack, referenced_ids};
use gitana_object_store::ObjectStoreError;
use gitana_repository::Repository;

use crate::GitHttpError;

/// Build a delta-compressed packfile carrying the objects reachable from `wants` but
/// not from `haves`. An empty want set yields an empty (header + trailer) pack.
///
/// Used server-side to answer a fetch, and client-side (`gta push`) to pack the
/// objects a push must send.
pub async fn build_pack<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
) -> Result<Vec<u8>, GitHttpError> {
	let objects = collect_objects(repo, wants, haves).await?;
	Ok(encode_pack(&objects))
}

/// Collect the objects to send: reachable-from-`wants` minus reachable-from-`haves`.
async fn collect_objects<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
) -> Result<Vec<PackedObject<H>>, GitHttpError> {
	let store = repo.objects();

	// Pass 1: mark everything reachable from the haves we actually have. An unknown
	// have is ignored (the client may report objects the server lacks).
	let mut excluded: HashSet<ObjectId<H>> = HashSet::new();
	let mut stack: Vec<ObjectId<H>> = haves.to_vec();
	while let Some(id) = stack.pop() {
		if !excluded.insert(id) {
			continue;
		}
		match store.read_object(&id).await {
			Ok((kind, data)) => stack.extend(referenced_ids::<H>(kind, &data)?),
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
		stack.extend(referenced_ids::<H>(kind, &data)?);
		result.push(PackedObject { id, kind, data });
	}
	Ok(result)
}
