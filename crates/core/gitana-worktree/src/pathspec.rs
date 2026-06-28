//! Pathspec normalisation shared by `add` and `restore`.
//!
//! A pathspec from the command line is interpreted relative to the directory the user invoked
//! the command from. `normalize` turns it into a canonical work-tree-relative path: it combines
//! the caller's `prefix` (a `/`-joined work-tree-relative subdirectory, empty at the root) with
//! the spec, then resolves `.` and `..` components against it. The current-dir forms `.` and
//! `./` collapse to the prefix itself (everything under the caller's directory). An empty
//! pathspec (`""`), and a `..` that climbs above the work-tree root, are rejected the way stock
//! git rejects them. A leading `/` (an absolute path) is also rejected, but here we differ from
//! git: git accepts an absolute path that points inside the work tree (relativising it), whereas
//! we only support worktree-relative pathspecs for now. Silently stripping the leading `/` and
//! treating it as relative would act on the wrong file, so rejecting is the safe choice.
//!
//! A trailing slash (`sub/`) or a final `.` component (`sub/.`) is reported via `dir_only`:
//! such a spec must resolve to a directory, the way `git checkout -- a.txt/` and
//! `git checkout -- a.txt/.` are rejected for a file. A final `..` is not directory-only — it
//! resolves to a parent the way git accepts `a.txt/..` (the directory above the file).

use crate::WorktreeError;

/// Returns the canonical worktree-relative path together with `dir_only` (the spec ended in a
/// slash or a `.` component and so may only match a directory).
pub(crate) fn normalize(spec: &str, prefix: &str) -> Result<(String, bool), WorktreeError> {
	if spec.starts_with('/') {
		return Err(WorktreeError::AbsolutePathspec(spec.to_owned()));
	}

	// Resolve the spec against the (already-canonical) prefix, applying `.`/`..` as we go.
	let mut stack: Vec<&str> = prefix.split('/').filter(|part| !part.is_empty()).collect();
	let mut named_a_path = false;
	let mut had_dot = false;
	for part in spec.split('/') {
		match part {
			"" => {}
			"." => had_dot = true,
			".." => {
				if stack.pop().is_none() {
					// Climbs above the work-tree root (e.g. `../x` at the root): outside the repo.
					return Err(WorktreeError::UnsafePath(spec.to_owned()));
				}
				named_a_path = true;
			}
			other => {
				stack.push(other);
				named_a_path = true;
			}
		}
	}

	// A spec of `.` / `./` means "everything under here"; `""` / `/` name nothing at all.
	if !named_a_path && !had_dot {
		return Err(WorktreeError::EmptyPathspec);
	}
	// A trailing slash, or a final `.` component (e.g. `a.txt/.`), means a directory is required.
	let last_named = spec.rsplit('/').find(|part| !part.is_empty());
	let dir_only = spec.ends_with('/') || last_named == Some(".");
	Ok((stack.join("/"), dir_only))
}
