use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use gitana_file_store::FileStore;
use gitana_object::{ObjectId, ObjectKind, TreeEntry, encode_tree, parse_tree};
use gitana_object_store::ObjectStore;

use crate::{FileMode, RepositoryError};

/// A flattened tree entry: a full repo-relative path, octal mode string, and id.
pub type FlatEntry = (String, String, ObjectId);

/// Recursively flatten a tree into `(path, mode, oid)` entries (`ls-tree -r`).
pub(crate) async fn read_tree_recursive(
	objects: &ObjectStore<impl FileStore>,
	tree: ObjectId,
) -> Result<Vec<FlatEntry>, RepositoryError> {
	let mut out = Vec::new();
	walk_tree(objects, tree, String::new(), &mut out).await?;
	Ok(out)
}

fn walk_tree<'a>(
	objects: &'a ObjectStore<impl FileStore>,
	tree: ObjectId,
	prefix: String,
	out: &'a mut Vec<FlatEntry>,
) -> Pin<Box<dyn Future<Output = Result<(), RepositoryError>> + 'a>> {
	Box::pin(async move {
		let (kind, payload) = objects.read_object(&tree).await?;
		if kind != ObjectKind::Tree {
			return Err(RepositoryError::InvalidRef(format!("{tree} is not a tree")));
		}
		for entry in parse_tree(&payload)? {
			let path = if prefix.is_empty() {
				entry.name
			} else {
				format!("{prefix}/{}", entry.name)
			};
			if entry.mode == FileMode::Directory.as_str() {
				walk_tree(objects, entry.id, path, out).await?;
			} else {
				out.push((path, entry.mode, entry.id));
			}
		}
		Ok(())
	})
}

/// A flat entry for [`crate::Repository::write_tree`]: a repo-relative path, its
/// file mode, and the blob (or submodule) id it points at.
#[derive(Debug, Clone)]
pub struct TreeBuildEntry {
	/// Path relative to the repository root, using `/` separators.
	pub path: String,
	/// The entry's file mode (not [`FileMode::Directory`] — directories are implied).
	pub mode: FileMode,
	/// The object id the entry points at.
	pub id: ObjectId,
}

/// An in-memory tree being assembled before any object is written.
#[derive(Default)]
struct Node {
	files: Vec<(String, FileMode, ObjectId)>,
	dirs: BTreeMap<String, Node>,
}

fn insert(node: &mut Node, path: &str, mode: FileMode, id: ObjectId) {
	match path.split_once('/') {
		None => node.files.push((path.to_owned(), mode, id)),
		Some((head, rest)) => insert(
			node.dirs.entry(head.to_owned()).or_default(),
			rest,
			mode,
			id,
		),
	}
}

/// Build the nested tree objects for `entries` and return the root tree id.
pub(crate) async fn build_tree(
	objects: &ObjectStore<impl FileStore>,
	entries: &[TreeBuildEntry],
) -> Result<ObjectId, RepositoryError> {
	let mut root = Node::default();
	for entry in entries {
		insert(&mut root, &entry.path, entry.mode, entry.id);
	}
	write_node(objects, root).await
}

fn write_node<'a>(
	objects: &'a ObjectStore<impl FileStore>,
	node: Node,
) -> Pin<Box<dyn Future<Output = Result<ObjectId, RepositoryError>> + 'a>> {
	Box::pin(async move {
		let mut entries: Vec<TreeEntry> = Vec::new();
		for (name, mode, id) in node.files {
			entries.push(TreeEntry {
				mode: mode.as_str().to_owned(),
				name,
				id,
			});
		}
		for (name, child) in node.dirs {
			let id = write_node(objects, child).await?;
			entries.push(TreeEntry {
				mode: FileMode::Directory.as_str().to_owned(),
				name,
				id,
			});
		}
		let payload = encode_tree(&entries);
		Ok(objects.write_object(ObjectKind::Tree, &payload).await?)
	})
}
