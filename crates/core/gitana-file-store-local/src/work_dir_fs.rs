use std::io;

use gitana_path::{GitPath, GitPathComponent};

use crate::{FileKind, Meta, WorkEntryIdentity};

/// One entry of a directory listing: its name (a single path component) and its kind, from an
/// `lstat` that does not follow symlinks (so a symlinked subdirectory reads as [`FileKind::Symlink`],
/// not [`FileKind::Dir`] — matching how git walks the working tree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
	/// The entry's name, a single `/`-free path component.
	pub name: GitPathComponent,
	/// The entry's kind (not following symlinks).
	pub kind: FileKind,
}

/// One entry to add while populating an empty or partially populated work tree.
///
/// [`WorkDirFs::populate_new`] creates every missing parent through retained, no-follow directory
/// capabilities and refuses to replace the final name. This is deliberately narrower than the
/// normal checkout primitives: it is for recovery and initial attachment, where preserving any
/// concurrently added content takes priority over making the target tree authoritative.
pub enum PopulationEntry<'a> {
	/// A directory, including the opaque directory that represents a gitlink.
	Directory,
	/// A regular file with its complete content and executable-bit intent.
	Regular {
		/// The file content.
		bytes: &'a [u8],
		/// Whether the executable bit should be set on platforms that represent it.
		executable: bool,
	},
	/// A symbolic link with the target bytes stored in the tree.
	Symlink(&'a [u8]),
}

/// A working-tree directory as a capability: all access is relative to a confined root (the work
/// tree), with no ambient authority. The native implementation `CapWorkDir` wraps a
/// `cap_std::fs::Dir`; the wasm implementation `DescriptorWorkDir` wraps a `wasi:filesystem`
/// directory descriptor. (Both are named as plain code spans, not intra-doc links: each is
/// `cfg`-gated to a single target, so a link would dangle when documenting the other.)
///
/// Paths are work-tree-relative and `/`-separated. Operations are synchronous — the underlying
/// syscalls (cap-std, or positional WASI descriptor calls) are themselves synchronous, and the
/// working-tree code that drives them already interleaves them with `await`ed object-store reads.
///
/// Confinement is structural: the capability resolves each relative path against its own root and
/// refuses to escape it at the syscall boundary, so this trait carries no path-sanitising of its own
/// (the working tree still applies its lexical `validate_path` guard against `..`/`.git`/traversal
/// before calling in).
pub trait WorkDirFs: Send + Sync + 'static {
	/// Whether this backend represents tree symlinks as regular files containing the target bytes.
	/// Population verification uses this to accept the platform fallback it just requested from
	/// [`Self::populate_new`] without weakening normal checkout comparisons.
	fn uses_symlink_placeholders(&self) -> bool {
		false
	}

	/// Validate that `path` can be represented by this filesystem backend without performing any
	/// filesystem access. Mutating operations use this as a batch preflight so a deterministic path
	/// encoding failure cannot occur after an earlier path has already been changed.
	fn validate_path_representable(&self, path: &GitPath) -> io::Result<()>;

	/// `lstat` the entry at `path` (not following a final symlink). `Ok(None)` when nothing is there
	/// — both "no such entry" and "a non-directory occupies an ancestor" (`ENOENT`/`ENOTDIR`) fold to
	/// `None`, since either way `path` names nothing.
	fn lstat(&self, path: &GitPath) -> io::Result<Option<Meta>>;

	/// Read the whole contents of the regular file at `path`.
	fn read(&self, path: &GitPath) -> io::Result<Vec<u8>>;

	/// Read the target of the symlink at `path`, as raw bytes (git stores it verbatim as the blob).
	fn read_link(&self, path: &GitPath) -> io::Result<Vec<u8>>;

	/// List the immediate entries of the directory at `path` (`""` = the work-tree root).
	fn read_dir(&self, path: &GitPath) -> io::Result<Vec<DirEntry>>;

	/// Write `bytes` as the regular file at `path`, replacing whatever plain file is there, and set
	/// the executable bit iff `executable`. Parent directories must already exist. A capability that
	/// cannot represent the executable bit (WASI) silently writes a non-executable file.
	fn write(&self, path: &GitPath, bytes: &[u8], executable: bool) -> io::Result<()>;

	/// Add one population entry without following any parent symlink or replacing the final name.
	/// Missing parents are created component-by-component. Returns `false` when the final name is
	/// already occupied; structural parent conflicts are returned as I/O errors. A failed regular-file
	/// write fails closed without removing a name that may have been replaced concurrently. Native
	/// backends retire their private prepared leaf under an internal quarantine name; WASI creates the
	/// final leaf exclusively and preserves a partial leaf when descriptor writes fail because the
	/// platform has no identity-conditioned unlink.
	fn populate_new(&self, path: &GitPath, entry: PopulationEntry<'_>) -> io::Result<bool>;

	/// [`Self::populate_new`], recording the entry created, and a digest of what it wrote, for a
	/// later [`Self::remove_file_if_identity`]: `Ok(Some(identity))` when created, `Ok(None)` when
	/// the name is already occupied. The native backend records a file or symlink as it prepared it,
	/// before giving it the name, so the identity is the entry created whatever happens at the name
	/// afterwards (a directory is read from its name). The default creates and then reads the
	/// name, leaving an instant in which another entry put there would be recorded instead.
	fn populate_new_recorded(
		&self,
		path: &GitPath,
		entry: PopulationEntry<'_>,
	) -> io::Result<Option<WorkEntryIdentity>> {
		let content = crate::work_entry_identity::population_content(&entry);
		if !self.populate_new(path, entry)? {
			return Ok(None);
		}
		let identity = self.entry_identity(path)?.ok_or_else(|| {
			io::Error::new(
				io::ErrorKind::NotFound,
				"the entry created was removed before it could be recorded",
			)
		})?;
		Ok(Some(match content {
			Some(content) => identity.with_content(content),
			None => identity,
		}))
	}

	/// Create a symlink at `path` pointing at `target` (raw bytes, as stored in the blob).
	fn symlink(&self, target: &[u8], path: &GitPath) -> io::Result<()>;

	/// Create the single directory `path` (its parent must already exist).
	fn create_dir(&self, path: &GitPath) -> io::Result<()>;

	/// Rename `from` to `to` within the work tree.
	fn rename(&self, from: &GitPath, to: &GitPath) -> io::Result<()>;

	/// Remove the file or symlink at `path`.
	fn remove_file(&self, path: &GitPath) -> io::Result<()>;

	/// Remove the empty directory at `path`.
	fn remove_dir(&self, path: &GitPath) -> io::Result<()>;

	/// Remove the directory at `path` and everything under it.
	fn remove_dir_all(&self, path: &GitPath) -> io::Result<()>;

	/// Record which entry `path` holds now (not following a final symlink), and as it is, for a
	/// later [`Self::remove_file_if_identity`]; `Ok(None)` when nothing is there. The default is an
	/// `lstat` snapshot ([`WorkEntryIdentity::from_meta`]).
	fn entry_identity(&self, path: &GitPath) -> io::Result<Option<WorkEntryIdentity>> {
		Ok(
			self
				.lstat(path)?
				.map(|meta| WorkEntryIdentity::from_meta(&meta)),
		)
	}

	/// Remove the file or symlink at `path` only while it still holds the entry `expected`
	/// recorded, unchanged (and with the content written, when [`Self::populate_new_recorded`]
	/// recorded it): `Ok(true)` when removed, `Ok(false)` when nothing is there or something else
	/// is, which is left as it is. The native backend renames the entry to a private name in
	/// its folder, checks it there (renaming it back when it differs) and then removes it, so an
	/// entry another program puts at `path` meanwhile is never removed. The default compares an
	/// `lstat` and then removes, leaving an instant between the two.
	fn remove_file_if_identity(
		&self,
		path: &GitPath,
		expected: &WorkEntryIdentity,
	) -> io::Result<bool> {
		crate::work_entry_identity::remove_if_meta_matches(self, path, expected)
	}
}
