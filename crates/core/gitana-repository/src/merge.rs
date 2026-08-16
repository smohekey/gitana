//! Three-way tree merge — the engine `merge` / `cherry-pick` / `revert` / `rebase` build on.
//!
//! Given a common `base` tree and two divergent trees `ours` and `theirs`, [`merge_trees`] produces
//! a merged tree and the list of conflicted paths. It recurses over tree levels; for a file changed
//! on both sides it runs a diff3 line merge ([`gitana_diff::merge`]), emitting conflict markers and
//! recording the path when the change does not merge cleanly. Rename detection and attribute merge
//! drivers are out of scope; binary clashes, mode clashes, modify/delete, and directory/file
//! clashes are reported as conflicts (keeping a deterministic side so the output tree stays valid).

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, TreeEntry, encode_tree, parse_tree};

use crate::{FileMode, Repository, RepositoryError};

/// The outcome of a three-way tree merge: the merged tree and the conflicted paths (sorted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeMerge<H: HashAlgorithm> {
	pub tree: ObjectId<H>,
	pub conflicts: Vec<String>,
}

/// Three-way merge the `ours` and `theirs` trees against their common `base` tree.
pub(crate) async fn merge_trees<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	base: ObjectId<H>,
	ours: ObjectId<H>,
	theirs: ObjectId<H>,
) -> Result<TreeMerge<H>, RepositoryError> {
	let (id, mut conflicts) = merge_tree(repo, Some(base), Some(ours), Some(theirs)).await?;
	conflicts.sort();
	let tree = match id {
		Some(id) => id,
		// Everything merged away: the result is the empty tree.
		None => write_tree(repo, &[]).await?,
	};
	Ok(TreeMerge { tree, conflicts })
}

/// Boxed recursive future for [`merge_tree`]: the merged subtree id (`None` if empty) and the
/// conflicted paths relative to that subtree.
type MergeTreeFuture<'a, H> = Pin<
	Box<dyn Future<Output = Result<(Option<ObjectId<H>>, Vec<String>), RepositoryError>> + Send + 'a>,
>;

/// How an entry of a given name appears on one side of the merge.
enum Side<'a, H: HashAlgorithm> {
	Absent,
	Blob(&'a TreeEntry<H>),
	Tree(&'a TreeEntry<H>),
	/// A submodule (gitlink, mode 160000): the id is a commit in the submodule, not a blob here. The
	/// entry itself is carried by `ours_entry`/`theirs_entry` at the conflict fallback, so no field.
	Gitlink,
}

fn classify<H: HashAlgorithm>(entry: Option<&TreeEntry<H>>) -> Side<'_, H> {
	match entry {
		None => Side::Absent,
		Some(entry) if entry.mode == FileMode::Directory.as_str() => Side::Tree(entry),
		// A gitlink is NOT a blob — never feed its commit id to blob merging (`read_blob` would fail with
		// "object not found", since a submodule's commit lives in the submodule). When both sides move the
		// pointer differently it falls to the conflict fallback below, which flags the path so the caller
		// records base/ours/theirs as `160000` conflict stages, exactly as git does.
		Some(entry) if entry.mode == FileMode::Gitlink.as_str() => Side::Gitlink,
		Some(entry) => Side::Blob(entry),
	}
}

/// Merge one tree level, returning the merged tree id (`None` if it has no entries, so a parent can
/// omit it) and the conflicted paths *relative to this subtree*.
fn merge_tree<'a, H: HashAlgorithm>(
	repo: &'a Repository<impl FileStore, H>,
	base: Option<ObjectId<H>>,
	ours: Option<ObjectId<H>>,
	theirs: Option<ObjectId<H>>,
) -> MergeTreeFuture<'a, H> {
	Box::pin(async move {
		let base_entries = read_entries(repo, base).await?;
		let ours_entries = read_entries(repo, ours).await?;
		let theirs_entries = read_entries(repo, theirs).await?;
		let base_map = by_name(&base_entries);
		let ours_map = by_name(&ours_entries);
		let theirs_map = by_name(&theirs_entries);

		let mut names: BTreeSet<&str> = BTreeSet::new();
		names.extend(base_map.keys().copied());
		names.extend(ours_map.keys().copied());
		names.extend(theirs_map.keys().copied());

		let mut entries: Vec<TreeEntry<H>> = Vec::new();
		let mut conflicts: Vec<String> = Vec::new();

		for name in names {
			let base_entry = base_map.get(name).copied();
			let ours_entry = ours_map.get(name).copied();
			let theirs_entry = theirs_map.get(name).copied();

			// Trivial resolutions first.
			let resolved = if ours_entry == theirs_entry {
				ours_entry // identical on both sides (incl. both deleted)
			} else if ours_entry == base_entry {
				theirs_entry // only theirs changed (incl. deleted)
			} else if theirs_entry == base_entry {
				ours_entry // only ours changed
			} else {
				// Both sides changed; resolve by type.
				match (classify(ours_entry), classify(theirs_entry)) {
					(Side::Tree(ours_sub), Side::Tree(theirs_sub)) => {
						let base_sub = match classify(base_entry) {
							Side::Tree(entry) => Some(entry.id),
							_ => None,
						};
						let (id, sub_conflicts) =
							merge_tree(repo, base_sub, Some(ours_sub.id), Some(theirs_sub.id)).await?;
						conflicts.extend(
							sub_conflicts
								.into_iter()
								.map(|path| format!("{name}/{path}")),
						);
						if let Some(id) = id {
							entries.push(tree_entry(name, id));
						}
						continue;
					}
					(Side::Blob(ours_blob), Side::Blob(theirs_blob)) => {
						let base_blob = match classify(base_entry) {
							Side::Blob(entry) => Some(entry),
							_ => None,
						};
						let entry = merge_blobs(
							repo,
							name,
							base_blob,
							ours_blob,
							theirs_blob,
							&mut conflicts,
						)
						.await?;
						entries.push(entry);
						continue;
					}
					// Directory/file, modify/delete, etc.: keep a deterministic side, flag a conflict.
					_ => {
						conflicts.push(name.to_owned());
						ours_entry.or(theirs_entry)
					}
				}
			};

			if let Some(entry) = resolved {
				entries.push(entry.clone());
			}
		}

		if entries.is_empty() {
			Ok((None, conflicts))
		} else {
			Ok((Some(write_tree(repo, &entries).await?), conflicts))
		}
	})
}

/// Three-way merge two blobs whose tree entries both differ from base. Content and mode resolve
/// independently: the content by blob identity (so a side that only changed the mode keeps the
/// other side's content), falling back to a diff3 line merge when both sides changed the content —
/// or, when that content is binary, to a conflict that keeps ours (git's merge-tree does the same).
/// The mode resolves three-way via [`resolve_mode`].
async fn merge_blobs<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	name: &str,
	base: Option<&TreeEntry<H>>,
	ours: &TreeEntry<H>,
	theirs: &TreeEntry<H>,
	conflicts: &mut Vec<String>,
) -> Result<TreeEntry<H>, RepositoryError> {
	let base_id = base.map(|entry| entry.id);
	let (id, content_conflict) = if ours.id == theirs.id || Some(theirs.id) == base_id {
		(ours.id, false) // theirs did not change the content (or both sides match)
	} else if Some(ours.id) == base_id {
		(theirs.id, false) // only theirs changed the content
	} else {
		// Both sides changed the content; line-merge it, unless it is binary.
		let base_content = match base {
			Some(entry) => repo.read_blob(entry.id).await?,
			None => Vec::new(),
		};
		let ours_content = repo.read_blob(ours.id).await?;
		let theirs_content = repo.read_blob(theirs.id).await?;
		if gitana_diff::is_binary(&base_content)
			|| gitana_diff::is_binary(&ours_content)
			|| gitana_diff::is_binary(&theirs_content)
		{
			(ours.id, true) // binary can't be line-merged: keep ours, conflict
		} else {
			let outcome = gitana_diff::merge(
				&base_content,
				&ours_content,
				&theirs_content,
				"ours",
				"theirs",
			);
			let id = repo
				.objects()
				.write_object(ObjectKind::Blob, &outcome.content)
				.await?;
			(id, outcome.conflicted)
		}
	};

	let (mode, mode_conflict) = resolve_mode(&ours.mode, &theirs.mode, base.map(|b| b.mode.as_str()));
	if content_conflict || mode_conflict {
		conflicts.push(name.to_owned());
	}
	Ok(TreeEntry {
		mode,
		name: name.to_owned(),
		id,
	})
}

/// Resolve a file mode three-way; the bool is whether the modes conflicted.
fn resolve_mode(ours: &str, theirs: &str, base: Option<&str>) -> (String, bool) {
	if ours == theirs {
		(ours.to_owned(), false)
	} else if base == Some(ours) {
		(theirs.to_owned(), false)
	} else if base == Some(theirs) {
		(ours.to_owned(), false)
	} else {
		(ours.to_owned(), true)
	}
}

async fn read_entries<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	tree: Option<ObjectId<H>>,
) -> Result<Vec<TreeEntry<H>>, RepositoryError> {
	let Some(tree) = tree else {
		return Ok(Vec::new());
	};
	let (kind, payload) = repo.objects().read_object(&tree).await?;
	if kind != ObjectKind::Tree {
		return Err(RepositoryError::InvalidRef(format!("{tree} is not a tree")));
	}
	Ok(parse_tree::<H>(&payload)?)
}

fn by_name<H: HashAlgorithm>(entries: &[TreeEntry<H>]) -> HashMap<&str, &TreeEntry<H>> {
	entries.iter().map(|e| (e.name.as_str(), e)).collect()
}

fn tree_entry<H: HashAlgorithm>(name: &str, id: ObjectId<H>) -> TreeEntry<H> {
	TreeEntry {
		mode: FileMode::Directory.as_str().to_owned(),
		name: name.to_owned(),
		id,
	}
}

async fn write_tree<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	entries: &[TreeEntry<H>],
) -> Result<ObjectId<H>, RepositoryError> {
	Ok(
		repo
			.objects()
			.write_object(ObjectKind::Tree, &encode_tree(entries))
			.await?,
	)
}

#[cfg(test)]
mod tests {
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::Sha256;
	use gitana_object_store::ObjectStore;

	use super::*;
	use crate::TreeBuildEntry;

	type Repo = Repository<MemoryFileStore, Sha256>;

	fn new_repo() -> Repo {
		Repository::new(ObjectStore::new(MemoryFileStore::new()))
	}

	/// Build a tree from `(path, content)` files (all regular blobs).
	async fn tree(repo: &Repo, files: &[(&str, &str)]) -> ObjectId<Sha256> {
		let mut entries = Vec::new();
		for (path, content) in files {
			let id = repo.write_blob(content.as_bytes()).await.unwrap();
			entries.push(TreeBuildEntry {
				path: (*path).to_owned(),
				mode: FileMode::Regular,
				id,
			});
		}
		repo.write_tree(&entries).await.unwrap()
	}

	/// A tree with a single `f.bin` blob of raw bytes (regular mode).
	async fn bin_tree(repo: &Repo, content: &[u8]) -> ObjectId<Sha256> {
		file_tree(repo, FileMode::Regular, content).await
	}

	/// A tree with a single `f.bin` blob of raw bytes and an explicit mode.
	async fn file_tree(repo: &Repo, mode: FileMode, content: &[u8]) -> ObjectId<Sha256> {
		let id = repo.write_blob(content).await.unwrap();
		repo
			.write_tree(&[TreeBuildEntry {
				path: "f.bin".to_owned(),
				mode,
				id,
			}])
			.await
			.unwrap()
	}

	async fn paths(repo: &Repo, tree: ObjectId<Sha256>) -> Vec<String> {
		let mut paths: Vec<String> = repo
			.read_tree(tree)
			.await
			.unwrap()
			.into_iter()
			.map(|(path, _, _)| path)
			.collect();
		paths.sort();
		paths
	}

	async fn file(repo: &Repo, tree: ObjectId<Sha256>, path: &str) -> Option<String> {
		for (entry_path, _, id) in repo.read_tree(tree).await.unwrap() {
			if entry_path == path {
				return Some(String::from_utf8(repo.read_blob(id).await.unwrap()).unwrap());
			}
		}
		None
	}

	#[tokio::test]
	async fn independent_additions_on_each_side() {
		let repo = new_repo();
		let base = tree(&repo, &[("keep.txt", "x\n")]).await;
		let ours = tree(&repo, &[("keep.txt", "x\n"), ("a.txt", "a\n")]).await;
		let theirs = tree(&repo, &[("keep.txt", "x\n"), ("b.txt", "b\n")]).await;

		let merged = merge_trees(&repo, base, ours, theirs).await.unwrap();
		assert!(merged.conflicts.is_empty());
		assert_eq!(
			paths(&repo, merged.tree).await,
			["a.txt", "b.txt", "keep.txt"]
		);
	}

	#[tokio::test]
	async fn disjoint_line_edits_merge_cleanly() {
		let repo = new_repo();
		let base = tree(&repo, &[("f.txt", "1\n2\n3\n4\n5\n")]).await;
		let ours = tree(&repo, &[("f.txt", "A\n2\n3\n4\n5\n")]).await;
		let theirs = tree(&repo, &[("f.txt", "1\n2\n3\n4\nB\n")]).await;

		let merged = merge_trees(&repo, base, ours, theirs).await.unwrap();
		assert!(merged.conflicts.is_empty());
		assert_eq!(
			file(&repo, merged.tree, "f.txt").await.unwrap(),
			"A\n2\n3\n4\nB\n"
		);
	}

	#[tokio::test]
	async fn same_line_diverging_conflicts() {
		let repo = new_repo();
		let base = tree(&repo, &[("f.txt", "1\n2\n3\n")]).await;
		let ours = tree(&repo, &[("f.txt", "1\nX\n3\n")]).await;
		let theirs = tree(&repo, &[("f.txt", "1\nY\n3\n")]).await;

		let merged = merge_trees(&repo, base, ours, theirs).await.unwrap();
		assert_eq!(merged.conflicts, ["f.txt"]);
		let content = file(&repo, merged.tree, "f.txt").await.unwrap();
		assert!(content.contains("<<<<<<< ours") && content.contains(">>>>>>> theirs"));
	}

	#[tokio::test]
	async fn modify_delete_conflicts_keeping_the_modification() {
		let repo = new_repo();
		let base = tree(&repo, &[("f.txt", "1\n2\n3\n")]).await;
		let ours = tree(&repo, &[("f.txt", "1\nMODIFIED\n3\n")]).await;
		let theirs = tree(&repo, &[]).await; // theirs deleted f.txt

		let merged = merge_trees(&repo, base, ours, theirs).await.unwrap();
		assert_eq!(merged.conflicts, ["f.txt"]);
		assert_eq!(
			file(&repo, merged.tree, "f.txt").await.unwrap(),
			"1\nMODIFIED\n3\n"
		);
	}

	#[tokio::test]
	async fn changes_in_a_shared_subdirectory_recurse() {
		let repo = new_repo();
		let base = tree(&repo, &[("dir/keep.txt", "x\n")]).await;
		let ours = tree(&repo, &[("dir/keep.txt", "x\n"), ("dir/a.txt", "a\n")]).await;
		let theirs = tree(&repo, &[("dir/keep.txt", "x\n"), ("dir/b.txt", "b\n")]).await;

		let merged = merge_trees(&repo, base, ours, theirs).await.unwrap();
		assert!(merged.conflicts.is_empty());
		assert_eq!(
			paths(&repo, merged.tree).await,
			["dir/a.txt", "dir/b.txt", "dir/keep.txt"]
		);
	}

	#[tokio::test]
	async fn divergent_binary_conflicts_and_keeps_ours_uncorrupted() {
		let repo = new_repo();
		let base = bin_tree(&repo, b"A\0base\n").await;
		let ours = bin_tree(&repo, b"A\0OURS\n").await;
		let theirs = bin_tree(&repo, b"A\0THEIRS\n").await;

		let merged = merge_trees(&repo, base, ours, theirs).await.unwrap();
		assert_eq!(merged.conflicts, ["f.bin"]);
		// The binary payload is kept as ours, not rewritten with conflict markers.
		let (_, _, id) = repo
			.read_tree(merged.tree)
			.await
			.unwrap()
			.into_iter()
			.find(|(path, _, _)| path == "f.bin")
			.unwrap();
		assert_eq!(repo.read_blob(id).await.unwrap(), b"A\0OURS\n");
	}

	#[tokio::test]
	async fn binary_mode_and_content_change_on_opposite_sides_merge_cleanly() {
		let repo = new_repo();
		// base is a regular binary file; ours only flips it executable, theirs only changes content.
		let base = file_tree(&repo, FileMode::Regular, b"A\0base\n").await;
		let ours = file_tree(&repo, FileMode::Executable, b"A\0base\n").await;
		let theirs = file_tree(&repo, FileMode::Regular, b"A\0THEIRS\n").await;

		let merged = merge_trees(&repo, base, ours, theirs).await.unwrap();
		assert!(
			merged.conflicts.is_empty(),
			"unexpected: {:?}",
			merged.conflicts
		);
		// Mode from ours, content from theirs — the changes don't overlap.
		let (_, mode, id) = repo
			.read_tree(merged.tree)
			.await
			.unwrap()
			.into_iter()
			.find(|(path, _, _)| path == "f.bin")
			.unwrap();
		assert_eq!(mode, "100755");
		assert_eq!(repo.read_blob(id).await.unwrap(), b"A\0THEIRS\n");
	}
}
