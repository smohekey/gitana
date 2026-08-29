use cap_std::fs::Dir;
use gitana_fs_native::directory_identity;

/// Compare two open directory capabilities using a stable filesystem identity.
///
/// This does not resolve either directory through an ambient path, so a concurrent rename or
/// replacement cannot redirect the comparison to another filesystem object.
pub fn same_directory_identity(left: &Dir, right: &Dir) -> std::io::Result<bool> {
	Ok(directory_identity(left)? == directory_identity(right)?)
}

#[cfg(test)]
mod tests {
	use cap_std::{ambient_authority, fs::Dir};

	use super::same_directory_identity;

	#[test]
	fn reopened_directory_has_the_same_identity() {
		let temporary = tempfile::tempdir().unwrap();
		let first = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
		let second = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();

		assert!(same_directory_identity(&first, &second).unwrap());
	}

	#[test]
	fn different_directories_have_different_identities() {
		let first_path = tempfile::tempdir().unwrap();
		let second_path = tempfile::tempdir().unwrap();
		let first = Dir::open_ambient_dir(first_path.path(), ambient_authority()).unwrap();
		let second = Dir::open_ambient_dir(second_path.path(), ambient_authority()).unwrap();

		assert!(!same_directory_identity(&first, &second).unwrap());
	}
}
