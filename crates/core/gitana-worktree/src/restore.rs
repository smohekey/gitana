//! Restore specific paths into the working tree (the `git checkout -- <paths>` family).
//!
//! Unlike `checkout`, which materialises a whole tree and prunes anything absent from it,
//! `restore` only touches the paths a pathspec selects and never moves `HEAD`. The source is
//! either a tree (which also updates the matching index entries) or the index itself (which
//! only rewrites the working-tree files). Path-safety handling is shared with `checkout` via
//! its `write_entry` helper.
//!
//! Like stock Git's path checkout, this discards uncommitted working-tree changes to the
//! selected paths — that is the operation's purpose — so there is no dirty-file guard here
//! (that guard belongs to branch switching, not path restore).

use gitana_file_store::FileStore;
use gitana_object::ObjectId;

use crate::checkout::write_entry;
use crate::pathspec::normalize;
use crate::{WorkTree, WorktreeError};

pub(crate) async fn run<F>(
	wt: &WorkTree<F>,
	source: Option<ObjectId>,
	pathspecs: &[&str],
	prefix: &str,
) -> Result<(), WorktreeError>
where
	F: FileStore,
{
	let mut index = wt.load_index()?;

	// The candidate `(path, mode, oid)` entries a pathspec may select.
	let candidates: Vec<(String, String, ObjectId)> = match source {
		Some(tree) => wt.repository().read_tree(tree).await?,
		None => index
			.entries
			.iter()
			.filter(|e| e.stage == 0)
			.map(|e| (e.path.clone(), format!("{:o}", e.mode), e.oid))
			.collect(),
	};

	// Select the entries each pathspec matches; a pathspec that matches nothing is an error.
	let mut selected: Vec<&(String, String, ObjectId)> = Vec::new();
	for &spec in pathspecs {
		let (normalized, dir_only) = normalize(spec, prefix)?;
		let mut matched = false;
		for candidate in &candidates {
			if matches(&candidate.0, &normalized, dir_only) {
				selected.push(candidate);
				matched = true;
			}
		}
		if !matched {
			return Err(WorktreeError::PathspecMatch(spec.to_owned()));
		}
	}

	// Drop index entries whose shape conflicts with a selected path (a file replacing a
	// directory, or vice versa), then write the new entries. The working-tree shape change is
	// handled lazily by `write_entry`/`ensure_parents`; restore has no removal pass, so the
	// stale index entries must be cleared here.
	for (path, _, _) in &selected {
		index.remove_type_conflicts(path);
	}
	for (path, mode, oid) in &selected {
		write_entry(wt, path, mode, *oid, &mut index).await?;
	}

	wt.save_index(&index)
}

/// Whether `path` is matched by an already-normalised `spec`. The empty spec matches
/// everything; otherwise it matches any path beneath it as a directory, and — unless the spec
/// required a directory (`dir_only`, a trailing slash) — the path exactly.
fn matches(path: &str, spec: &str, dir_only: bool) -> bool {
	if spec.is_empty() {
		return true;
	}
	(!dir_only && path == spec) || path.starts_with(&format!("{spec}/"))
}
