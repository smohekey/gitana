use std::io;
use std::path::Path;

use gitana_file_store_local::{CapWorkDir, DirEntry, LocalFileStore, Meta, WorkDirFs};
use gitana_object::Sha256;
use gitana_object_store::ObjectStore;
use gitana_path::GitPath;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

fn open_dir(path: impl AsRef<Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

/// A Unix test capability with the pathname constraints of the WASI/non-Unix backends. The inner
/// capability can perform raw-byte operations, so any attempted mutation before a complete
/// representability preflight remains observable on disk.
pub struct Utf8OnlyWorkDir(CapWorkDir);

impl WorkDirFs for Utf8OnlyWorkDir {
	fn validate_path_representable(&self, path: &GitPath) -> io::Result<()> {
		path.as_utf8().map(drop).ok_or_else(|| {
			io::Error::new(
				io::ErrorKind::Unsupported,
				"test backend requires UTF-8 paths",
			)
		})
	}

	fn lstat(&self, path: &GitPath) -> io::Result<Option<Meta>> {
		self.0.lstat(path)
	}

	fn read(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		self.0.read(path)
	}

	fn read_link(&self, path: &GitPath) -> io::Result<Vec<u8>> {
		self.0.read_link(path)
	}

	fn read_dir(&self, path: &GitPath) -> io::Result<Vec<DirEntry>> {
		self.0.read_dir(path)
	}

	fn write(&self, path: &GitPath, bytes: &[u8], executable: bool) -> io::Result<()> {
		self.0.write(path, bytes, executable)
	}

	fn symlink(&self, target: &[u8], path: &GitPath) -> io::Result<()> {
		self.0.symlink(target, path)
	}

	fn create_dir(&self, path: &GitPath) -> io::Result<()> {
		self.0.create_dir(path)
	}

	fn rename(&self, from: &GitPath, to: &GitPath) -> io::Result<()> {
		self.0.rename(from, to)
	}

	fn remove_file(&self, path: &GitPath) -> io::Result<()> {
		self.0.remove_file(path)
	}

	fn remove_dir(&self, path: &GitPath) -> io::Result<()> {
		self.0.remove_dir(path)
	}

	fn remove_dir_all(&self, path: &GitPath) -> io::Result<()> {
		self.0.remove_dir_all(path)
	}
}

pub fn make_utf8_only_repo(work: &Path) -> WorkTree<LocalFileStore, Utf8OnlyWorkDir, Sha256> {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(
		repo,
		Utf8OnlyWorkDir(CapWorkDir::from_dir(open_dir(work))),
		git_dir,
	)
}
