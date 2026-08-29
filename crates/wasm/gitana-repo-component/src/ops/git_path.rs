//! Lossless conversions between the WIT path DTO and the engine path type.

use crate::bindings::exports::gitana::repo::porcelain::{
	GitPath as WitGitPath, RepoError, RevisionSpec as WitRevisionSpec,
};

pub(crate) fn from_wit(path: WitGitPath) -> Result<gitana_path::GitPath, RepoError> {
	let bytes = match path {
		WitGitPath::Utf8(path) => path.into_bytes(),
		WitGitPath::Bytes(path) => path,
	};
	gitana_path::GitPath::from_bytes(bytes)
		.map_err(|error| RepoError::Invalid(format!("invalid Git path: {error}")))
}

pub(crate) fn into_wit(path: gitana_path::GitPath) -> WitGitPath {
	match path.as_utf8() {
		Some(path) => WitGitPath::Utf8(path.to_owned()),
		None => WitGitPath::Bytes(path.into_bytes()),
	}
}

pub(crate) fn tree_path_into_wit(path: gitana_path::GitTreePath) -> WitGitPath {
	match path.as_utf8() {
		Some(path) => WitGitPath::Utf8(path.to_owned()),
		None => WitGitPath::Bytes(path.into_bytes()),
	}
}

pub(crate) fn revision_from_wit(spec: WitRevisionSpec) -> Result<Vec<u8>, RepoError> {
	let bytes = match spec {
		WitRevisionSpec::Utf8(spec) => spec.into_bytes(),
		WitRevisionSpec::Bytes(spec) => spec,
	};
	if bytes.contains(&0) {
		return Err(RepoError::Invalid(
			"revision specifications cannot contain NUL bytes".to_owned(),
		));
	}
	Ok(bytes)
}

pub(crate) fn display_revision(spec: &[u8]) -> String {
	gitana_path::GitPathspec::from_bytes(spec.to_vec())
		.map_or_else(|_| format!("{spec:?}"), |spec| spec.quote())
}

pub(crate) fn display_path(path: &gitana_path::GitPath) -> String {
	path.quote_with_affixes(b"", b"")
}

/// Convert a WIT path for a filesystem-backed operation. The current WASI
/// filesystem API accepts only Unicode path strings, so raw non-UTF-8 Git
/// bytes are rejected explicitly instead of being replaced or misdirected.
pub(crate) fn into_worktree_text(path: &WitGitPath) -> Result<String, RepoError> {
	match path {
		WitGitPath::Utf8(path) => Ok(path.clone()),
		WitGitPath::Bytes(path) => String::from_utf8(path.clone()).map_err(|_| {
			RepoError::UnsupportedFormat(
				"this WASI filesystem cannot represent a non-UTF-8 Git path".to_owned(),
			)
		}),
	}
}
