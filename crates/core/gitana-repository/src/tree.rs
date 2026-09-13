use std::{collections::BTreeMap, future::Future, pin::Pin};

use gitana_file_store::FileStore;
use gitana_object::{
	HashAlgorithm, ObjectError, ObjectId, ObjectKind, TreeEntry, encode_tree, parse_tree,
};
use gitana_object_store::ObjectStore;
use gitana_path::{GitPath, GitPathComponent, GitTreePath};

use crate::{FileMode, RepositoryError};

/// A flattened tree entry: a full repo-relative path, octal mode string, and id.
pub type FlatEntry<H> = (GitPath, String, ObjectId<H>);

/// A structurally flattened tree entry whose path may contain noncanonical tree names.
pub type RawFlatEntry<H> = (GitTreePath, String, ObjectId<H>);

/// Recursively flatten a tree into `(path, mode, oid)` entries (`ls-tree -r`).
pub(crate) async fn read_tree_recursive<F: FileStore, H: HashAlgorithm>(
	objects: &ObjectStore<F, H>,
	tree: ObjectId<H>,
) -> Result<Vec<FlatEntry<H>>, RepositoryError> {
	read_tree_recursive_raw(objects, tree)
		.await?
		.into_iter()
		.map(|(path, mode, id)| {
			let path = GitPath::try_from(path).map_err(|_| ObjectError::InvalidTreeName)?;
			Ok((path, mode, id))
		})
		.collect::<Result<_, ObjectError>>()
		.map_err(Into::into)
}

/// Recursively flatten a structurally readable tree without imposing filesystem path invariants.
pub(crate) async fn read_tree_recursive_raw<F: FileStore, H: HashAlgorithm>(
	objects: &ObjectStore<F, H>,
	tree: ObjectId<H>,
) -> Result<Vec<RawFlatEntry<H>>, RepositoryError> {
	let mut out = Vec::new();
	walk_tree(objects, tree, None, &mut out).await?;
	Ok(out)
}

fn walk_tree<'a, F: FileStore, H: HashAlgorithm>(
	objects: &'a ObjectStore<F, H>,
	tree: ObjectId<H>,
	prefix: Option<GitTreePath>,
	out: &'a mut Vec<RawFlatEntry<H>>,
) -> Pin<Box<dyn Future<Output = Result<(), RepositoryError>> + Send + 'a>> {
	Box::pin(async move {
		let (kind, payload) = objects.read_object(&tree).await?;
		if kind != ObjectKind::Tree {
			return Err(RepositoryError::InvalidRef(format!("{tree} is not a tree")));
		}
		for entry in parse_tree::<H>(&payload)? {
			let path = prefix.as_ref().map_or_else(
				|| GitTreePath::from_entry_name(&entry.name),
				|prefix| prefix.join(&entry.name),
			);
			if entry.mode == FileMode::Directory.as_str() {
				walk_tree(objects, entry.id, Some(path), out).await?;
			} else {
				out.push((path, entry.mode, entry.id));
			}
		}
		Ok(())
	})
}

/// A flat entry for [`crate::Repository::write_tree`]: a repo-relative path, its
/// mode, and the object id it points at. Directories may either be implied by a
/// nested path or supplied explicitly with [`FileMode::Directory`].
#[derive(Debug, Clone)]
pub struct TreeBuildEntry<H: HashAlgorithm> {
	/// Path relative to the repository root, using `/` separators.
	pub path: GitPath,
	/// The entry's file mode.
	pub mode: FileMode,
	/// The object id the entry points at.
	pub id: ObjectId<H>,
}

/// An in-memory tree being assembled before any object is written.
struct Node<H: HashAlgorithm> {
	leaves: BTreeMap<GitPathComponent, (FileMode, ObjectId<H>)>,
	dirs: BTreeMap<GitPathComponent, Node<H>>,
}

impl<H: HashAlgorithm> Default for Node<H> {
	fn default() -> Self {
		Node {
			leaves: BTreeMap::new(),
			dirs: BTreeMap::new(),
		}
	}
}

fn insert<H: HashAlgorithm>(
	node: &mut Node<H>,
	path: &GitPath,
	mode: FileMode,
	id: ObjectId<H>,
) -> Result<(), RepositoryError> {
	let components: Vec<GitPathComponent> = path
		.components()
		.map(|component| GitPathComponent::from_bytes(component.to_vec()))
		.collect::<Result<_, _>>()
		.map_err(|_| {
			RepositoryError::InvalidTree(format!("invalid repository-relative path {path}"))
		})?;
	if components.is_empty() {
		return Err(RepositoryError::InvalidTree(format!(
			"invalid repository-relative path {path}"
		)));
	}

	let mut current = node;
	for component in &components[..components.len() - 1] {
		if current.leaves.contains_key(component) {
			return Err(RepositoryError::InvalidTree(format!(
				"path component {component:?} is already a file"
			)));
		}
		current = current.dirs.entry(component.clone()).or_default();
	}

	let name = &components[components.len() - 1];
	if current.dirs.contains_key(name) {
		return Err(RepositoryError::InvalidTree(format!(
			"path {path:?} is already a directory"
		)));
	}
	if current.leaves.insert(name.clone(), (mode, id)).is_some() {
		return Err(RepositoryError::InvalidTree(format!(
			"duplicate path {path:?}"
		)));
	}

	Ok(())
}

struct PreparedTree<H: HashAlgorithm> {
	root: ObjectId<H>,
	objects: Vec<(ObjectId<H>, Vec<u8>)>,
}

fn prepare_tree<H: HashAlgorithm>(
	entries: &[TreeBuildEntry<H>],
) -> Result<PreparedTree<H>, RepositoryError> {
	let mut root = Node::default();
	for entry in entries {
		insert(&mut root, &entry.path, entry.mode, entry.id)?;
	}

	let mut objects = Vec::new();
	let root = encode_node(root, &mut objects);
	Ok(PreparedTree { root, objects })
}

fn encode_node<H: HashAlgorithm>(
	node: Node<H>,
	objects: &mut Vec<(ObjectId<H>, Vec<u8>)>,
) -> ObjectId<H> {
	let mut entries = Vec::new();
	for (name, (mode, id)) in node.leaves {
		entries.push(TreeEntry {
			mode: mode.as_str().to_owned(),
			name: name.into(),
			id,
		});
	}
	for (name, child) in node.dirs {
		entries.push(TreeEntry {
			mode: FileMode::Directory.as_str().to_owned(),
			name: name.into(),
			id: encode_node(child, objects),
		});
	}

	let payload = encode_tree(&entries);
	let id = ObjectId::compute(ObjectKind::Tree, &payload);
	objects.push((id, payload));
	id
}

/// Compute the root id for the canonical nested tree represented by `entries`.
///
/// This performs the same path validation and encoding as
/// [`crate::Repository::write_tree`] without reading or writing a repository.
pub fn compute_tree_id<H: HashAlgorithm>(
	entries: &[TreeBuildEntry<H>],
) -> Result<ObjectId<H>, RepositoryError> {
	Ok(prepare_tree(entries)?.root)
}

/// Build the nested tree objects for `entries` and return the root tree id.
pub(crate) async fn build_tree<F: FileStore, H: HashAlgorithm>(
	objects: &ObjectStore<F, H>,
	entries: &[TreeBuildEntry<H>],
) -> Result<ObjectId<H>, RepositoryError> {
	let prepared = prepare_tree(entries)?;
	for (expected, payload) in prepared.objects {
		let written = objects.write_object(ObjectKind::Tree, &payload).await?;
		debug_assert_eq!(written, expected);
	}
	Ok(prepared.root)
}

#[cfg(test)]
mod tests {
	use super::*;
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::Sha256;

	use crate::Repository;

	fn entry(path: &str) -> TreeBuildEntry<Sha256> {
		TreeBuildEntry {
			path: GitPath::from_utf8(path).unwrap(),
			mode: FileMode::Regular,
			id: ObjectId::compute(ObjectKind::Blob, path.as_bytes()),
		}
	}

	#[test]
	fn compute_tree_id_is_stable_across_input_order() {
		let first = compute_tree_id(&[entry("a"), entry("dir/b")]).unwrap();
		let second = compute_tree_id(&[entry("dir/b"), entry("a")]).unwrap();
		assert_eq!(first, second);
	}

	#[test]
	fn path_type_rejects_invalid_paths() {
		for path in ["/a", "a/", "a//b", ".", "..", "a/../b", "a\0b"] {
			assert!(GitPath::from_utf8(path).is_err());
		}
		assert!(GitPath::from_utf8("").unwrap().is_root());
	}

	#[test]
	fn accepts_an_explicit_directory_leaf() {
		let child = compute_tree_id::<Sha256>(&[]).unwrap();
		let explicit = compute_tree_id(&[TreeBuildEntry {
			path: GitPath::from_utf8("dir").unwrap(),
			mode: FileMode::Directory,
			id: child,
		}])
		.unwrap();
		let nested = compute_tree_id(&[TreeBuildEntry {
			path: GitPath::from_utf8("dir/file").unwrap(),
			mode: FileMode::Regular,
			id: ObjectId::compute(ObjectKind::Blob, b"file"),
		}])
		.unwrap();
		assert_ne!(explicit, nested);
	}

	#[test]
	fn rejects_duplicate_and_file_directory_conflicts() {
		assert!(matches!(
			compute_tree_id(&[entry("same"), entry("same")]),
			Err(RepositoryError::InvalidTree(_))
		));
		assert!(matches!(
			compute_tree_id(&[entry("a"), entry("a/b")]),
			Err(RepositoryError::InvalidTree(_))
		));
		assert!(matches!(
			compute_tree_id(&[entry("a/b"), entry("a")]),
			Err(RepositoryError::InvalidTree(_))
		));
	}

	#[tokio::test]
	async fn raw_walk_preserves_noncanonical_names_while_canonical_walk_rejects_them() {
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(MemoryFileStore::new()));
		let blob = repo.write_blob(b"content").await.unwrap();
		let mut payload = b"100644 .\0".to_vec();
		payload.extend_from_slice(blob.as_bytes());
		let tree = repo
			.objects()
			.write_object(ObjectKind::Tree, &payload)
			.await
			.unwrap();

		let raw = repo.read_tree_raw(tree).await.unwrap();
		assert_eq!(raw.len(), 1);
		assert_eq!(raw[0].0.as_bytes(), b".");
		assert_eq!(raw[0].1, "100644");
		assert_eq!(raw[0].2, blob);
		assert!(matches!(
			repo.read_tree(tree).await,
			Err(RepositoryError::Object(ObjectError::InvalidTreeName))
		));
	}
}
