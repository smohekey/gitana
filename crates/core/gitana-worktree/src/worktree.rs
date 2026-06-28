use std::io::Write;
use std::path::{Path, PathBuf};

use gitana_file_store::FileStore;
use gitana_repository::Repository;

use crate::fsmeta::{file_mode, mode_of, path_bytes, stat_of};
use crate::ignore::{self, DirIgnore};
use crate::{Index, IndexEntry, Status, WorktreeError};

/// A working directory paired with its repository.
///
/// Filesystem-coupled by nature: working-tree files and the index are real files,
/// so it reads/writes them with `std::fs`, while blob objects go through the
/// repository's object store. The index is written with git's `index.lock`
/// create-new-then-rename protocol. `add`/`status`/`checkout` build on this.
pub struct WorkTree<F> {
	repo: Repository<F>,
	work_dir: PathBuf,
	git_dir: PathBuf,
}

impl<F: FileStore> WorkTree<F> {
	/// Build a working tree over `repo`, with the working directory at `work_dir`
	/// and the git directory at `git_dir`.
	pub fn new(
		repo: Repository<F>,
		work_dir: impl Into<PathBuf>,
		git_dir: impl Into<PathBuf>,
	) -> Self {
		Self {
			repo,
			work_dir: work_dir.into(),
			git_dir: git_dir.into(),
		}
	}

	/// The underlying repository.
	pub fn repository(&self) -> &Repository<F> {
		&self.repo
	}

	pub(crate) fn work_dir(&self) -> &Path {
		&self.work_dir
	}

	/// Read and parse `.git/index`, or an empty index if it does not exist.
	pub fn load_index(&self) -> Result<Index, WorktreeError> {
		match std::fs::read(self.git_dir.join("index")) {
			Ok(bytes) => Ok(Index::parse(&bytes)?),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Index::new()),
			Err(error) => Err(error.into()),
		}
	}

	/// Write the index via `index.lock` create-new + rename (git's protocol).
	pub fn save_index(&self, index: &Index) -> Result<(), WorktreeError> {
		let lock = self.git_dir.join("index.lock");
		let mut file = match std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&lock)
		{
			Ok(file) => file,
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
				return Err(WorktreeError::IndexLocked);
			}
			Err(error) => return Err(error.into()),
		};
		if let Err(error) = file.write_all(&index.write_v4()) {
			let _ = std::fs::remove_file(&lock);
			return Err(error.into());
		}
		drop(file);
		std::fs::rename(&lock, self.git_dir.join("index"))?;
		Ok(())
	}

	/// Stage `pathspecs`, interpreted relative to `prefix` (a `/`-joined work-tree-relative
	/// subdirectory, empty at the root). A file is staged directly; a directory (or `.`) is
	/// walked, applying `.gitignore`, and its non-ignored files are staged; a path that no
	/// longer exists is removed from the index (a staged deletion).
	pub async fn add(&self, pathspecs: &[&str], prefix: &str) -> Result<(), WorktreeError> {
		let mut index = self.load_index()?;
		let mut ignore_stack: Vec<DirIgnore> = Vec::new();
		for &spec in pathspecs {
			let (rel, dir_only) = crate::pathspec::normalize(spec, prefix)?;
			let full = if rel.is_empty() {
				self.work_dir.clone()
			} else {
				self.work_dir.join(&rel)
			};
			match std::fs::symlink_metadata(&full) {
				Ok(meta) if meta.is_dir() && !meta.is_symlink() => {
					let mut files = Vec::new();
					walk_files(&full, &rel, &mut ignore_stack, &mut files)?;
					for file in files {
						self.stage_file(&mut index, &file).await?;
					}
				}
				// A trailing-slash spec required a directory but resolved to a file or nothing.
				Ok(_) if dir_only => return Err(WorktreeError::PathspecMatch(spec.to_owned())),
				Ok(_) => self.stage_file(&mut index, &rel).await?,
				Err(error) if error.kind() == std::io::ErrorKind::NotFound && dir_only => {
					return Err(WorktreeError::PathspecMatch(spec.to_owned()));
				}
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => index.remove(&rel),
				Err(error) => return Err(error.into()),
			}
		}
		self.save_index(&index)
	}

	async fn stage_file(&self, index: &mut Index, path: &str) -> Result<(), WorktreeError> {
		let full = self.work_dir.join(path);
		match std::fs::symlink_metadata(&full) {
			Ok(meta) if meta.is_symlink() => {
				let target = std::fs::read_link(&full)?;
				let oid = self.repo.write_blob(path_bytes(&target)).await?;
				index.remove_type_conflicts(path);
				index.upsert(entry(path, 0o120000, oid, &meta));
			}
			Ok(meta) if meta.is_file() => {
				let content = std::fs::read(&full)?;
				let oid = self.repo.write_blob(&content).await?;
				index.remove_type_conflicts(path);
				index.upsert(entry(path, file_mode(&meta), oid, &meta));
			}
			Ok(_) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => index.remove(path),
			Err(error) => return Err(error.into()),
		}
		Ok(())
	}

	/// Compute the three-way status: HEAD tree vs index (staged) and index vs
	/// working tree (unstaged), plus untracked files.
	pub async fn status(&self) -> Result<Status, WorktreeError> {
		crate::status::compute(self).await
	}

	/// Content changes between the index and the working tree (`git diff`).
	pub async fn diff_unstaged(&self) -> Result<Vec<crate::FileDiff>, WorktreeError> {
		crate::diff::unstaged(self).await
	}

	/// Content changes between the `HEAD` tree and the index (`git diff --cached`).
	pub async fn diff_staged(&self) -> Result<Vec<crate::FileDiff>, WorktreeError> {
		crate::diff::staged(self).await
	}

	/// Materialise `tree` into the working directory and index. Without `force`,
	/// refuses to overwrite uncommitted local changes. Does not move `HEAD`.
	pub async fn checkout(
		&self,
		tree: gitana_object::ObjectId,
		force: bool,
	) -> Result<(), WorktreeError> {
		crate::checkout::run(self, tree, force).await
	}

	/// Restore `pathspecs` from `source` (a tree; `None` = the current index) into the chosen
	/// targets — the working tree (`worktree`) and/or the index (`staged`) — discarding any
	/// uncommitted changes to those paths. A selected path absent from the source but currently
	/// tracked is removed from the chosen targets. Does not move `HEAD`. `pathspecs` are
	/// interpreted relative to `prefix` (a `/`-joined work-tree-relative subdirectory, empty
	/// at the root).
	pub async fn restore(
		&self,
		source: Option<gitana_object::ObjectId>,
		worktree: bool,
		staged: bool,
		pathspecs: &[&str],
		prefix: &str,
	) -> Result<(), WorktreeError> {
		crate::restore::run(self, source, worktree, staged, pathspecs, prefix).await
	}
}

fn entry(
	path: &str,
	mode: u32,
	oid: gitana_object::ObjectId,
	meta: &std::fs::Metadata,
) -> IndexEntry {
	IndexEntry {
		stat: stat_of(meta),
		mode,
		oid,
		stage: 0,
		assume_valid: false,
		path: path.to_owned(),
	}
}

/// Whether a working-tree file matches an index entry by its stat cache and mode
/// (the fast path that avoids re-hashing).
pub(crate) fn stat_matches(entry: &IndexEntry, meta: &std::fs::Metadata) -> bool {
	entry.mode == mode_of(meta) && entry.stat == stat_of(meta)
}

/// Collect all non-ignored files under `dir_path` (recursively), applying
/// `.gitignore` and skipping `.git`. Used to expand a directory pathspec for `add`.
fn walk_files(
	dir_path: &Path,
	dir_rel: &str,
	stack: &mut Vec<DirIgnore>,
	out: &mut Vec<String>,
) -> Result<(), WorktreeError> {
	let pushed = match std::fs::read_to_string(dir_path.join(".gitignore")) {
		Ok(text) => {
			stack.push(ignore::parse(&text, dir_rel));
			true
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
		Err(error) => return Err(error.into()),
	};

	for entry in std::fs::read_dir(dir_path)? {
		let entry = entry?;
		let name = entry.file_name();
		let name = name.to_string_lossy();
		if name == ".git" {
			continue;
		}
		let rel = if dir_rel.is_empty() {
			name.into_owned()
		} else {
			format!("{dir_rel}/{name}")
		};
		let is_dir = entry.metadata()?.is_dir();
		if ignore::is_ignored(&rel, is_dir, stack) {
			continue;
		}
		if is_dir {
			walk_files(&entry.path(), &rel, stack, out)?;
		} else {
			out.push(rel);
		}
	}

	if pushed {
		stack.pop();
	}
	Ok(())
}
