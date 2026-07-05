//! Object enumeration for pack generation: the set of objects reachable from a set
//! of `want` tips but not from a set of `have` tips.
//!
//! Pure graph walk: the caller supplies a reader closure that materialises an object
//! by id (the storage layer's job). Two passes — first mark everything reachable
//! from `have`s as uninteresting, then collect everything reachable from `want`s
//! that was not marked. Commits pull in their tree and parents, trees their entries,
//! tags their target; blobs are leaves. The returned objects feed
//! [`crate::encode_pack`] directly.

use std::collections::HashSet;

use crate::pack::PackedObject;
use crate::{
	HashAlgorithm, ObjectError, ObjectId, ObjectKind, parse_commit, parse_tag, parse_tree,
};

/// Collect the objects reachable from `wants` but not from `haves`.
///
/// `read` returns an object's kind and payload, or `None` if the id is unknown to
/// the source. Unknown `have`s are ignored (the client may report objects the
/// server lacks); an unknown object reached from a `want` is a connectivity gap and
/// fails with [`ObjectError::MissingObject`]. The result order is unspecified —
/// [`crate::encode_pack`] imposes its own.
pub fn enumerate_objects<H: HashAlgorithm, F>(
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
	mut read: F,
) -> Result<Vec<PackedObject<H>>, ObjectError>
where
	F: FnMut(ObjectId<H>) -> Result<Option<(ObjectKind, Vec<u8>)>, ObjectError>,
{
	// Pass 1: everything reachable from the haves is uninteresting.
	let mut excluded: HashSet<ObjectId<H>> = HashSet::new();
	let mut stack: Vec<ObjectId<H>> = haves.to_vec();
	while let Some(id) = stack.pop() {
		if !excluded.insert(id) {
			continue;
		}
		if let Some((kind, data)) = read(id)? {
			push_children(kind, &data, &mut stack)?;
		}
	}

	// Pass 2: collect reachable-from-wants minus the excluded set.
	let mut collected: HashSet<ObjectId<H>> = HashSet::new();
	let mut result: Vec<PackedObject<H>> = Vec::new();
	let mut stack: Vec<ObjectId<H>> = wants.to_vec();
	while let Some(id) = stack.pop() {
		if excluded.contains(&id) || !collected.insert(id) {
			continue;
		}
		let (kind, data) = read(id)?.ok_or(ObjectError::MissingObject)?;
		push_children(kind, &data, &mut stack)?;
		result.push(PackedObject { id, kind, data });
	}
	Ok(result)
}

/// Push the ids an object directly references onto the walk stack.
fn push_children<H: HashAlgorithm>(
	kind: ObjectKind,
	data: &[u8],
	stack: &mut Vec<ObjectId<H>>,
) -> Result<(), ObjectError> {
	stack.extend(referenced_ids(kind, data)?);
	Ok(())
}

/// The object ids an object directly references: a commit's tree and parents, a
/// tree's entries, an annotated tag's target; a blob references nothing.
///
/// The building block for any reachability walk — used by [`enumerate_objects`] here
/// and by the async pack builder in `gitana-git-http`.
pub fn referenced_ids<H: HashAlgorithm>(
	kind: ObjectKind,
	data: &[u8],
) -> Result<Vec<ObjectId<H>>, ObjectError> {
	let ids = match kind {
		ObjectKind::Commit => {
			let commit = parse_commit::<H>(data)?;
			let mut ids = Vec::with_capacity(commit.parents.len() + 1);
			ids.push(commit.tree);
			ids.extend(commit.parents);
			ids
		}
		ObjectKind::Tree => parse_tree::<H>(data)?
			.into_iter()
			.map(|entry| entry.id)
			.collect(),
		ObjectKind::Tag => vec![parse_tag::<H>(data)?.object],
		ObjectKind::Blob => Vec::new(),
	};
	Ok(ids)
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;
	use crate::{Commit, Sha256, TreeEntry, encode_commit, encode_tree};

	/// What the test reader yields for an id: its kind and payload, or `None`.
	type ReadResult = Result<Option<(ObjectKind, Vec<u8>)>, ObjectError>;

	/// A tiny in-memory object store for the walk tests.
	#[derive(Default)]
	struct Store {
		objects: HashMap<ObjectId<Sha256>, (ObjectKind, Vec<u8>)>,
	}

	impl Store {
		fn put(&mut self, kind: ObjectKind, data: Vec<u8>) -> ObjectId<Sha256> {
			let id = ObjectId::<Sha256>::compute(kind, &data);
			self.objects.insert(id, (kind, data));
			id
		}

		fn reader(&self) -> impl FnMut(ObjectId<Sha256>) -> ReadResult + '_ {
			move |id| Ok(self.objects.get(&id).cloned())
		}
	}

	fn commit(tree: ObjectId<Sha256>, parents: Vec<ObjectId<Sha256>>, msg: &str) -> Vec<u8> {
		encode_commit(&Commit {
			tree,
			parents,
			author: "A <a@x> 1 +0000".to_owned(),
			committer: "A <a@x> 1 +0000".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: msg.to_owned(),
		})
	}

	fn tree_with(name: &str, blob: ObjectId<Sha256>) -> Vec<u8> {
		encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: name.to_owned(),
			id: blob,
		}])
	}

	#[test]
	fn collects_full_history_with_no_haves() {
		let mut store = Store::default();
		let blob = store.put(ObjectKind::Blob, b"hello".to_vec());
		let tree = store.put(ObjectKind::Tree, tree_with("f", blob));
		let root = store.put(ObjectKind::Commit, commit(tree, vec![], "root"));

		let objects = enumerate_objects(&[root], &[], store.reader()).expect("enumerate");
		let ids: HashSet<ObjectId<Sha256>> = objects.iter().map(|o| o.id).collect();
		assert_eq!(ids, HashSet::from([blob, tree, root]));
	}

	#[test]
	fn excludes_objects_reachable_from_haves() {
		let mut store = Store::default();
		let blob1 = store.put(ObjectKind::Blob, b"v1".to_vec());
		let tree1 = store.put(ObjectKind::Tree, tree_with("f", blob1));
		let c1 = store.put(ObjectKind::Commit, commit(tree1, vec![], "c1"));

		let blob2 = store.put(ObjectKind::Blob, b"v2".to_vec());
		let tree2 = store.put(ObjectKind::Tree, tree_with("f", blob2));
		let c2 = store.put(ObjectKind::Commit, commit(tree2, vec![c1], "c2"));

		// have c1 → only the new commit, its tree and blob should be sent.
		let objects = enumerate_objects(&[c2], &[c1], store.reader()).expect("enumerate");
		let ids: HashSet<ObjectId<Sha256>> = objects.iter().map(|o| o.id).collect();
		assert_eq!(ids, HashSet::from([blob2, tree2, c2]));
	}

	#[test]
	fn shared_objects_between_branches_sent_once() {
		// A blob shared by the want and a have is excluded; an unshared blob is sent.
		let mut store = Store::default();
		let shared = store.put(ObjectKind::Blob, b"shared".to_vec());
		let have_tree = store.put(ObjectKind::Tree, tree_with("s", shared));
		let have = store.put(ObjectKind::Commit, commit(have_tree, vec![], "have"));

		let unique = store.put(ObjectKind::Blob, b"unique".to_vec());
		let want_tree = store.put(
			ObjectKind::Tree,
			encode_tree(&[
				TreeEntry {
					mode: "100644".to_owned(),
					name: "s".to_owned(),
					id: shared,
				},
				TreeEntry {
					mode: "100644".to_owned(),
					name: "u".to_owned(),
					id: unique,
				},
			]),
		);
		let want = store.put(ObjectKind::Commit, commit(want_tree, vec![], "want"));

		let objects = enumerate_objects(&[want], &[have], store.reader()).expect("enumerate");
		let ids: HashSet<ObjectId<Sha256>> = objects.iter().map(|o| o.id).collect();
		assert!(ids.contains(&unique));
		assert!(ids.contains(&want_tree));
		assert!(
			!ids.contains(&shared),
			"shared blob should be excluded by the have"
		);
	}

	#[test]
	fn unknown_want_object_is_a_missing_object_error() {
		let store = Store::default();
		let phantom = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"nope");
		assert!(matches!(
			enumerate_objects(&[phantom], &[], store.reader()),
			Err(ObjectError::MissingObject)
		));
	}

	#[test]
	fn unknown_have_is_ignored() {
		let mut store = Store::default();
		let blob = store.put(ObjectKind::Blob, b"x".to_vec());
		let tree = store.put(ObjectKind::Tree, tree_with("f", blob));
		let root = store.put(ObjectKind::Commit, commit(tree, vec![], "root"));
		let phantom = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"absent");

		// A have we don't have must not abort enumeration.
		let objects = enumerate_objects(&[root], &[phantom], store.reader()).expect("enumerate");
		assert_eq!(objects.len(), 3);
	}
}
