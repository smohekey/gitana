//! A path's attributes, from a stack of attributes files, resolved as git's `attr.c` resolves them.

use std::collections::HashMap;

use gitana_file_store::{FileStore, FileStoreError};
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::{GitPath, GitPathComponent};

use crate::{AttrDirection, AttrFile, AttrState, WorkTree, WorktreeError};

/// git ignores an attributes file larger than this (`ATTR_MAX_FILE_SIZE`).
const MAX_FILE_SIZE: usize = 100 * 1024 * 1024;

/// The attributes files for one operation, read as git reads them: the system and global files
/// handed to the [`WorkTree`], `info/attributes`, and each folder's `.gitattributes` (loaded once,
/// when a path below it is first asked about), from the working tree or from `indexed` (the index
/// view's `.gitattributes`, by folder) in the order `direction` says.
pub(crate) struct Attributes<H: HashAlgorithm> {
	outside: Vec<AttrFile>,
	info: Option<AttrFile>,
	indexed: HashMap<Vec<u8>, ObjectId<H>>,
	/// Folders whose index view's `.gitattributes` is a symlink the operation stages, by the link's
	/// target text, which git reads from its index as the file's rules.
	staged_links: HashMap<Vec<u8>, Vec<u8>>,
	/// Folders whose working-tree `.gitattributes` the operation removes, so it isn't read.
	removed: std::collections::HashSet<Vec<u8>>,
	direction: AttrDirection,
	folders: HashMap<Vec<u8>, Option<AttrFile>>,
	fold: bool,
}

impl<H: HashAlgorithm> Attributes<H> {
	/// Read the system, global and `info/attributes` files for `wt`. `indexed` holds the index
	/// view's entries (path, mode and object), of which its `.gitattributes` files are kept.
	pub(crate) async fn load<'a, F, W>(
		wt: &WorkTree<F, W, H>,
		indexed: impl IntoIterator<Item = (&'a GitPath, u32, ObjectId<H>)>,
		direction: AttrDirection,
	) -> Result<Self, WorktreeError>
	where
		F: FileStore,
		W: WorkDirFs,
	{
		let outside = wt.outside_attributes();
		let outside = [outside.system.as_deref(), outside.global.as_deref()]
			.into_iter()
			.flatten()
			.filter(|text| text.len() <= MAX_FILE_SIZE)
			.map(|text| AttrFile::parse(text, b"", true))
			.collect();
		let info = match wt
			.repository()
			.objects()
			.file_store()
			.read_path("info/attributes")
			.await
		{
			Ok(text) if text.len() <= MAX_FILE_SIZE => Some(AttrFile::parse(&text, b"", true)),
			Ok(_) | Err(FileStoreError::NotFound) => None,
			Err(error) => return Err(WorktreeError::Repository(error.into())),
		};
		// A submodule at a `.gitattributes` path isn't one: git reads the index's only when it holds a
		// blob, and counts anything else as absent.
		let indexed = indexed
			.into_iter()
			.filter(|(_, mode, _)| *mode != 0o160000)
			.filter_map(|(path, _, oid)| attributes_folder(path).map(|folder| (folder, oid)))
			.collect();
		Ok(Attributes {
			outside,
			info,
			indexed,
			staged_links: HashMap::new(),
			removed: std::collections::HashSet::new(),
			direction,
			folders: HashMap::new(),
			fold: crate::excludes::ignore_case(wt).await?,
		})
	}

	/// Stop reading the index view's `.gitattributes` at `path`, before any file under it is looked
	/// up: one an operation is removing from the index.
	pub(crate) fn forget_indexed(&mut self, path: &GitPath) {
		if let Some(folder) = attributes_folder(path) {
			self.indexed.remove(&folder);
			self.staged_links.remove(&folder);
		}
	}

	/// Read the index view's `.gitattributes` at `path` as `target`, before any file under it is
	/// looked up: a symlink the operation stages there, whose target text git reads from its index as
	/// the file's rules.
	pub(crate) fn stage_link(&mut self, path: &GitPath, target: Vec<u8>) {
		if let Some(folder) = attributes_folder(path) {
			self.staged_links.insert(folder, target);
		}
	}

	/// Stop reading the working tree's `.gitattributes` at `path`, before any file under it is looked
	/// up: one the operation removes, which git has removed before it writes the rest.
	pub(crate) fn forget_worktree(&mut self, path: &GitPath) {
		if let Some(folder) = attributes_folder(path) {
			self.removed.insert(folder);
		}
	}

	/// The attributes of the file `path`, as [`resolve`] gives them.
	pub(crate) async fn of<F, W>(
		&mut self,
		wt: &WorkTree<F, W, H>,
		path: &GitPath,
	) -> Result<HashMap<Vec<u8>, AttrState>, WorktreeError>
	where
		F: FileStore,
		W: WorkDirFs,
	{
		let mut folders = vec![GitPath::root()];
		folders.extend(
			path
				.strict_ancestors()
				.into_iter()
				.filter(|folder| !folder.is_root()),
		);
		for folder in &folders {
			if !self.folders.contains_key(folder.as_bytes()) {
				let file = self.read_folder(wt, folder).await?;
				self.folders.insert(folder.as_bytes().to_vec(), file);
			}
		}
		let mut levels: Vec<&AttrFile> = self.outside.iter().collect();
		for folder in &folders {
			if let Some(Some(file)) = self.folders.get(folder.as_bytes()) {
				levels.push(file);
			}
		}
		levels.extend(self.info.iter());
		Ok(resolve(&levels, path.as_bytes(), self.fold))
	}

	/// The `.gitattributes` of `folder`, from the working tree or the index view as `direction`
	/// says. A symlink in the working tree isn't followed, as git doesn't follow one.
	async fn read_folder<F, W>(
		&self,
		wt: &WorkTree<F, W, H>,
		folder: &GitPath,
	) -> Result<Option<AttrFile>, WorktreeError>
	where
		F: FileStore,
		W: WorkDirFs,
	{
		let path = folder.join(&attributes_name());
		let from_worktree = || -> Option<Vec<u8>> {
			if self.removed.contains(folder.as_bytes()) {
				return None;
			}
			let meta = wt.work().lstat(&path).ok()??;
			if !meta.kind.is_file() {
				return None;
			}
			wt.work().read(&path).ok()
		};
		// git reads an indexed attributes file as one C string: its first NUL ends the whole file
		// (measured against git 2.55; a file on disk only loses the rest of that line).
		let from_index = async || -> Result<Option<Vec<u8>>, WorktreeError> {
			if let Some(target) = self.staged_links.get(folder.as_bytes()) {
				return Ok(Some(target.clone()));
			}
			match self.indexed.get(folder.as_bytes()) {
				Some(oid) => {
					let mut text = wt.repository().read_blob(*oid).await?;
					if let Some(nul) = text.iter().position(|byte| *byte == 0) {
						text.truncate(nul);
					}
					Ok(Some(text))
				}
				None => Ok(None),
			}
		};
		let text = match self.direction {
			AttrDirection::CheckIn => match from_worktree() {
				Some(text) => Some(text),
				None => from_index().await?,
			},
			AttrDirection::CheckOut => match from_index().await? {
				Some(text) => Some(text),
				None => from_worktree(),
			},
		};
		Ok(
			text
				.filter(|text| text.len() <= MAX_FILE_SIZE)
				.map(|text| AttrFile::parse(&text, folder.as_bytes(), folder.is_root())),
		)
	}
}

/// The folder whose `.gitattributes` `path` is, by its bytes; `None` for any other file.
pub(crate) fn attributes_folder(path: &GitPath) -> Option<Vec<u8>> {
	if path.components().last() != Some(attributes_name().as_bytes()) {
		return None;
	}
	Some(
		path
			.strict_ancestors()
			.into_iter()
			.last()
			.map(|folder| folder.as_bytes().to_vec())
			.unwrap_or_default(),
	)
}

fn attributes_name() -> GitPathComponent {
	GitPathComponent::from_utf8(".gitattributes").expect("a valid path component")
}

/// git's built-in macro, the lowest definition of `binary`.
fn builtin_binary() -> Vec<(Vec<u8>, AttrState)> {
	vec![
		(b"diff".to_vec(), AttrState::Unset),
		(b"merge".to_vec(), AttrState::Unset),
		(b"text".to_vec(), AttrState::Unset),
	]
}

/// The attributes of the file `path` (relative to the working tree's root) from `levels`, lowest
/// precedence first: the system and global files, each `.gitattributes` from the root down, and
/// `info/attributes`. Every attribute a matching rule or an expanded macro gives; an attribute
/// missing from the map is unspecified. As git does: the highest file wins, and within a file the
/// last matching line; each attribute is decided by the first rule that names it, and a macro set
/// there expands in its place. `fold` folds ASCII case (`core.ignoreCase`).
pub(crate) fn resolve(
	levels: &[&AttrFile],
	path: &[u8],
	fold: bool,
) -> HashMap<Vec<u8>, AttrState> {
	let builtin = builtin_binary();
	let mut macros: HashMap<&[u8], &[(Vec<u8>, AttrState)]> = HashMap::new();
	for level in levels.iter().rev() {
		for definition in level.macros.iter().rev() {
			macros
				.entry(definition.name.as_slice())
				.or_insert(definition.attrs.as_slice());
		}
	}
	macros.entry(b"binary").or_insert(builtin.as_slice());
	let mut found = HashMap::new();
	for level in levels.iter().rev() {
		let Some(rel) = crate::ignore::strip_dir(path, &level.dir) else {
			continue;
		};
		for rule in level.rules.iter().rev() {
			if rule.matches(rel, fold) {
				fill(&mut found, &rule.attrs, &macros);
			}
		}
	}
	found
}

/// git's `fill_one`: each of `attrs`, last first, not yet decided, decides it; a macro set there
/// expands in its place.
fn fill(
	found: &mut HashMap<Vec<u8>, AttrState>,
	attrs: &[(Vec<u8>, AttrState)],
	macros: &HashMap<&[u8], &[(Vec<u8>, AttrState)]>,
) {
	for (name, state) in attrs.iter().rev() {
		if found.contains_key(name) {
			continue;
		}
		found.insert(name.clone(), state.clone());
		if *state == AttrState::Set
			&& let Some(expansion) = macros.get(name.as_slice())
		{
			fill(found, expansion, macros);
		}
	}
}

#[cfg(all(test, unix))]
mod tests {
	use std::process::Command;

	use super::*;

	fn git(dir: &std::path::Path, args: &[&str]) -> String {
		let out = Command::new("git")
			.arg("-C")
			.arg(dir)
			.args(args)
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_ATTR_NOSYSTEM", "1")
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"git {args:?}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8(out.stdout).unwrap()
	}

	/// What `git check-attr -a` gives for `path`: each set, unset or valued attribute.
	fn git_attrs(dir: &std::path::Path, path: &str) -> Vec<String> {
		let mut attrs: Vec<String> = git(dir, &["check-attr", "-a", "--", path])
			.lines()
			.map(|line| line.split_once(": ").unwrap().1.to_owned())
			.collect();
		attrs.sort();
		attrs
	}

	fn ours(levels: &[&AttrFile], path: &str, fold: bool) -> Vec<String> {
		let mut attrs: Vec<String> = resolve(levels, path.as_bytes(), fold)
			.into_iter()
			.filter_map(|(name, state)| {
				let value = match state {
					AttrState::Set => "set".to_owned(),
					AttrState::Unset => "unset".to_owned(),
					AttrState::Unspecified => return None,
					AttrState::Value(value) => String::from_utf8(value).unwrap(),
				};
				Some(format!("{}: {value}", String::from_utf8(name).unwrap()))
			})
			.collect();
		attrs.sort();
		attrs
	}

	const ROOT: &str = "\
# a comment
*.txt text
*.bin binary
docs/*.md eol=crlf
/top.c text=auto
build/ -text
\"quoted name.dat\" -text ident
!negated text
bad.c invalid@name
[attr]mine text eol=lf diff
*.mine mine
*.both text -text
*.dup eol=lf
*.dup eol=crlf
*.pre text diff eol=lf
*.pre -text=auto !diff=x
*.badpre text
*.badpre -bad@name=1
foo**/bar eol=lf
x**y eol=crlf
*.nul filter=demo\0junk eol=crlf
";
	const SUB: &str = "\
*.txt -text
deep/** eol=lf
[attr]ignored text
*.ig ignored
*.c !text
";
	const INFO: &str = "\
special.txt eol=crlf
";

	/// Every path's attributes match `git check-attr -a`, across a root and a subfolder
	/// `.gitattributes`, `info/attributes`, macros (one defined where git ignores it), the built-in
	/// `binary`, a quoted pattern, a negative pattern and a line with an invalid name (both ignored),
	/// a folder-only pattern, `!` resetting an attribute, `-`/`!` before a value (the value is
	/// ignored), `**` after a literal in its component (not spanning folders), and a NUL ending its
	/// line.
	#[test]
	fn resolves_as_git_check_attr_does() {
		let temp = tempfile::tempdir().unwrap();
		let work = temp.path();
		git(work, &["init", "-q"]);
		std::fs::write(work.join(".gitattributes"), ROOT).unwrap();
		std::fs::create_dir_all(work.join("sub/deep")).unwrap();
		std::fs::write(work.join("sub/.gitattributes"), SUB).unwrap();
		std::fs::write(work.join(".git/info/attributes"), INFO).unwrap();
		let root = AttrFile::parse(ROOT.as_bytes(), b"", true);
		let sub = AttrFile::parse(SUB.as_bytes(), b"sub", false);
		let info = AttrFile::parse(INFO.as_bytes(), b"", true);
		for path in [
			"a.txt",
			"x/a.txt",
			"sub/a.txt",
			"sub/deep/a.txt",
			"sub/deep/more/b.rs",
			"a.bin",
			"docs/r.md",
			"docs/x/r.md",
			"top.c",
			"x/top.c",
			"build",
			"build/out.o",
			"quoted name.dat",
			"negated",
			"bad.c",
			"f.mine",
			"sub/f.ig",
			"f.both",
			"f.dup",
			"f.pre",
			"f.badpre",
			"foo/nested/bar",
			"foox/bar",
			"x/y",
			"xay",
			"f.nul",
			"special.txt",
			"sub/special.txt",
			"sub/m.c",
			"plain",
		] {
			let levels: Vec<&AttrFile> = [&root, &sub, &info]
				.into_iter()
				.filter(|level| crate::ignore::strip_dir(path.as_bytes(), &level.dir).is_some())
				.collect();
			assert_eq!(ours(&levels, path, false), git_attrs(work, path), "{path}");
		}
	}

	/// With `core.ignoreCase`, patterns match regardless of ASCII case, as git's do.
	#[test]
	fn folds_case_as_git_does_with_ignore_case() {
		let temp = tempfile::tempdir().unwrap();
		let work = temp.path();
		git(work, &["init", "-q"]);
		git(work, &["config", "core.ignoreCase", "true"]);
		std::fs::write(
			work.join(".gitattributes"),
			"*.TXT text\nDocs/*.md eol=crlf\n",
		)
		.unwrap();
		let root = AttrFile::parse(b"*.TXT text\nDocs/*.md eol=crlf\n", b"", true);
		for path in ["a.txt", "A.TXT", "docs/r.md", "DOCS/R.MD"] {
			assert_eq!(ours(&[&root], path, true), git_attrs(work, path), "{path}");
		}
	}
}
