use std::{ffi::OsStr, path::Path};

use anyhow::Result;
use gitana_path::{GitPath, GitPathspec};

use crate::ResultPathMode;

pub fn bytes_from_os(value: &OsStr) -> Result<Vec<u8>> {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;

		Ok(value.as_bytes().to_vec())
	}

	#[cfg(not(unix))]
	{
		let Some(value) = value.to_str() else {
			anyhow::bail!("this platform cannot represent the supplied value as exact Git bytes");
		};
		Ok(value.as_bytes().to_vec())
	}
}

pub fn pathspec_from_os(value: &OsStr) -> Result<GitPathspec> {
	Ok(GitPathspec::from_bytes(bytes_from_os(value)?)?)
}

pub(crate) fn render_result_path(path: &GitPath, mode: ResultPathMode) -> String {
	match mode {
		ResultPathMode::Human => path.to_string(),
		ResultPathMode::Reversible => path.quote_with_affixes(b"", b""),
	}
}

pub(crate) fn render_result_pathspec(pathspec: &GitPathspec, mode: ResultPathMode) -> String {
	match mode {
		ResultPathMode::Human => pathspec.to_string(),
		ResultPathMode::Reversible => pathspec.quote(),
	}
}

/// Render an anyhow chain while retaining typed working-tree paths until this frontend boundary.
/// Other error causes keep anyhow's ordinary `cause: source` ordering.
pub fn render_error(error: &anyhow::Error, mode: ResultPathMode) -> String {
	let mut rendered = Vec::new();
	for cause in error.chain() {
		if let Some(sparse) = cause.downcast_ref::<crate::commands::SparseCheckoutError>() {
			rendered.push(sparse.render_with_paths(
				|path| render_result_path(path, mode),
				|pathspec| render_result_pathspec(pathspec, mode),
			));
			break;
		}
		if let Some(conflict) = cause.downcast_ref::<gitana_porcelain::ConflictOverwriteError>() {
			rendered.push(conflict.render_with_paths(|path| render_result_path(path, mode)));
			break;
		}
		if let Some(worktree) = cause.downcast_ref::<gitana_worktree::WorktreeError>() {
			rendered.push(worktree.render_with_paths(
				|path| render_result_path(path, mode),
				|pathspec| render_result_pathspec(pathspec, mode),
			));
			break;
		}
		if let Some(repository) = cause.downcast_ref::<gitana_repository::RepositoryError>() {
			rendered.push(repository.render_with_paths(
				|path| render_result_path(path, mode),
				|pathspec| render_result_pathspec(pathspec, mode),
			));
			break;
		}
		rendered.push(cause.to_string());
	}
	if rendered.is_empty() {
		format!("{error:#}")
	} else {
		rendered.join(": ")
	}
}

pub(crate) fn repository_path_from_native(value: &Path) -> Result<GitPath> {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;

		Ok(GitPath::from_bytes(value.as_os_str().as_bytes().to_vec())?)
	}

	#[cfg(not(unix))]
	{
		let Some(value) = value.to_str() else {
			anyhow::bail!("this platform cannot represent the supplied path as exact Git bytes");
		};
		Ok(GitPath::from_utf8(
			value.replace(std::path::MAIN_SEPARATOR, "/"),
		)?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn result_path_modes_preserve_human_utf8_and_make_raw_paths_injective() {
		let utf8 = GitPath::from_utf8("café").unwrap();
		assert_eq!(render_result_path(&utf8, ResultPathMode::Human), "café");

		let raw = GitPath::from_bytes(b"bad-\xff".to_vec()).unwrap();
		let literal_collision = GitPath::from_utf8("\"bad-\\377\"").unwrap();
		assert_eq!(
			render_result_path(&raw, ResultPathMode::Human),
			render_result_path(&literal_collision, ResultPathMode::Human)
		);
		assert_ne!(
			render_result_path(&raw, ResultPathMode::Reversible),
			render_result_path(&literal_collision, ResultPathMode::Reversible)
		);
	}

	#[test]
	fn conflict_overwrite_errors_use_the_frontend_path_mode() {
		let error =
			|path| anyhow::Error::new(gitana_porcelain::ConflictOverwriteError::new(vec![path]));
		let raw = error(GitPath::from_bytes(b"raw-\xff".to_vec()).unwrap());
		let literal = error(GitPath::from_utf8("\"raw-\\377\"").unwrap());

		assert_eq!(
			render_error(&raw, ResultPathMode::Human),
			render_error(&literal, ResultPathMode::Human)
		);
		assert_ne!(
			render_error(&raw, ResultPathMode::Reversible),
			render_error(&literal, ResultPathMode::Reversible)
		);
	}

	#[test]
	fn result_pathspec_modes_make_human_collisions_injective() {
		let raw = GitPathspec::from_bytes(b"raw-\xff".to_vec()).unwrap();
		let literal = GitPathspec::from_utf8("\"raw-\\377\"").unwrap();
		assert_eq!(
			render_result_pathspec(&raw, ResultPathMode::Human),
			render_result_pathspec(&literal, ResultPathMode::Human)
		);
		assert_ne!(
			render_result_pathspec(&raw, ResultPathMode::Reversible),
			render_result_pathspec(&literal, ResultPathMode::Reversible)
		);
	}
}
