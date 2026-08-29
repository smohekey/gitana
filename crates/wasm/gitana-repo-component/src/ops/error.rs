//! Engine → WIT error mapping.

use gitana_file_store::FileStoreError;
use gitana_object_store::ObjectStoreError;
use gitana_repository::RepositoryError;
use gitana_worktree::WorktreeError;

use crate::bindings::exports::gitana::repo::porcelain::RepoError;

/// Map engine errors onto the WIT `repo-error` surface. Anything without a more
/// precise variant is a `backend` failure.
pub(crate) fn repo_error(error: RepositoryError) -> RepoError {
	let rendered = error.render_with_paths(
		|path| path.quote_with_affixes(b"", b""),
		gitana_path::GitPathspec::quote,
	);
	match error {
		RepositoryError::UnknownRevision(spec) => RepoError::UnknownRevision(spec),
		RepositoryError::AmbiguousRevision(hex) => RepoError::Ambiguous(hex),
		RepositoryError::InvalidRef(message) => RepoError::Invalid(message),
		RepositoryError::InvalidRevisionEncoding(_)
		| RepositoryError::InvalidTreePath { .. }
		| RepositoryError::MissingTreePath { .. }
		| RepositoryError::TreePathComponentNotDirectory { .. }
		| RepositoryError::TreePathNotDirectory(_) => RepoError::Invalid(rendered),
		RepositoryError::RefMoved { name } => RepoError::RefMoved(name),
		RepositoryError::MissingObject(id) => RepoError::NotFound(format!("missing object {id}")),
		RepositoryError::UnsupportedFormat(message) => RepoError::UnsupportedFormat(message),
		RepositoryError::Object(error) => RepoError::Invalid(error.to_string()),
		RepositoryError::FileStore(error) => file_store_error(error),
		RepositoryError::ObjectStore(error) => object_store_error(error),
		_ => RepoError::Backend(rendered),
	}
}

fn object_store_error(error: ObjectStoreError) -> RepoError {
	match error {
		ObjectStoreError::NotFound => RepoError::NotFound("object not found".to_owned()),
		corruption @ ObjectStoreError::Corruption { .. } => {
			RepoError::Corruption(corruption.to_string())
		}
		// A too-large input is the caller's fault, not a storage failure.
		too_large @ ObjectStoreError::TooLarge { .. } => RepoError::Invalid(too_large.to_string()),
		ObjectStoreError::Object(error) => RepoError::Invalid(error.to_string()),
		ObjectStoreError::FileStore(error) => file_store_error(error),
	}
}

fn file_store_error(error: FileStoreError) -> RepoError {
	match error {
		FileStoreError::NotFound => RepoError::NotFound("not found".to_owned()),
		other => RepoError::Backend(other.to_string()),
	}
}

/// Map a working-tree error onto the WIT `repo-error` surface. Overwrite refusals
/// (`Conflict`/`UntrackedOverwrite`) map to `conflict`, unsafe or malformed inputs to
/// `invalid`; file-store and repository errors defer to their own mappings so
/// not-found/ref-moved stay precise.
pub(crate) fn worktree_error(error: WorktreeError) -> RepoError {
	let rendered = error.render_with_paths(
		|path| path.quote_with_affixes(b"", b""),
		gitana_path::GitPathspec::quote,
	);
	match error {
		WorktreeError::FileStore(error) => file_store_error(error),
		WorktreeError::Repository(error) => repo_error(error),
		WorktreeError::Io(error) if error.kind() == std::io::ErrorKind::Unsupported => {
			RepoError::UnsupportedFormat(error.to_string())
		}
		WorktreeError::Conflict(_) | WorktreeError::UntrackedOverwrite(_) => {
			RepoError::Conflict(rendered)
		}
		WorktreeError::ChecksumMismatch => RepoError::Corruption("index checksum mismatch".to_owned()),
		WorktreeError::Malformed(_)
		| WorktreeError::UnsafePath(_)
		| WorktreeError::UnsafePathspec(_)
		| WorktreeError::InvalidPath(_)
		| WorktreeError::PathspecMatch(_)
		| WorktreeError::EmptyPathspec
		| WorktreeError::AbsolutePathspec(_)
		| WorktreeError::IndexPathMissing(..)
		| WorktreeError::InvalidIndexSpec(_)
		// An out-of-cone `mv`, an `add` advisory (out-of-cone and/or ignored pathspecs), and a malformed
		// sparse config value are caller/state errors, not backend failures — surface them as `invalid`.
		| WorktreeError::SparsePathExcluded(_)
		| WorktreeError::PathspecAdvisory { .. }
		| WorktreeError::InvalidPathspecMagic(_)
		// Submodule (gitlink) rejections are caller/state errors, not backend failures: a pathspec naming
		// into a submodule, an unresolvable/no-HEAD or null-OID gitlink, or a symlink/special node at a
		// mount slot. `backend` is reserved for underlying file-store faults, so surface these as `invalid`.
		| WorktreeError::PathspecInSubmodule { .. }
		| WorktreeError::SubmoduleNoCommit(_)
		| WorktreeError::SubmodulePathIsSymlink(_)
		| WorktreeError::NullGitlinkOid(_)
		| WorktreeError::UnsupportedFileType(_)
		| WorktreeError::Config(_) => RepoError::Invalid(rendered),
		_ => RepoError::Backend(rendered),
	}
}

#[cfg(test)]
mod tests {
	use std::io;

	use gitana_worktree::WorktreeError;

	use super::{RepoError, worktree_error};

	#[test]
	fn unsupported_worktree_io_maps_to_unsupported_format() {
		let mapped = worktree_error(WorktreeError::Io(io::Error::new(
			io::ErrorKind::Unsupported,
			"Git path is not representable on this host",
		)));

		match mapped {
			RepoError::UnsupportedFormat(message) => {
				assert_eq!(message, "Git path is not representable on this host");
			}
			other => panic!("expected unsupported-format, got {other:?}"),
		}
	}

	#[test]
	fn other_worktree_io_remains_a_backend_error() {
		let mapped = worktree_error(WorktreeError::Io(io::Error::new(
			io::ErrorKind::PermissionDenied,
			"permission denied",
		)));

		assert!(matches!(mapped, RepoError::Backend(_)));
	}
}
