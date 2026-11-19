//! What git's `convert.c` does to content crossing between the working tree and the repository,
//! for one operation: line endings converted as the attributes and config say, and the
//! conversions gitana won't do (a filter program, `working-tree-encoding`, `ident`) refused.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::SystemTime;

use gitana_config::GitConfig;
use gitana_file_store::{FileStore, FileStoreError};
use gitana_file_store_local::{Meta, WorkDirFs};
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind};
use gitana_path::GitPath;
use gitana_repository::RepositoryError;

use crate::fsmeta::file_mode;
use crate::{
	AttrDirection, AttrState, Attributes, EolConfig, Index, IndexEntry, LineEndingChange, WorkTree,
	WorktreeError,
};

/// A working-tree file as the repository would store it.
pub(crate) enum Staged<'c> {
	/// Its content, and the line-ending change git's `core.safecrlf=warn` warns of, if any.
	Content(Cow<'c, [u8]>, Option<LineEndingChange>),
	/// It needs a conversion gitana won't do: the attribute that asks for it, as written.
	Unconvertible(String),
}

/// Content conversion for one operation over `wt`.
pub(crate) struct Converter<'a, F, W, H: HashAlgorithm> {
	wt: &'a WorkTree<F, W, H>,
	attributes: Attributes<H>,
	direction: AttrDirection,
	/// The index file's time, which says which entries' stat can vouch for them.
	index_time: Option<SystemTime>,
	eol: EolConfig,
	config: GitConfig,
	/// Each staged file's line-ending warning, and whether the index already had the file.
	warnings: Vec<(bool, GitPath, LineEndingChange)>,
	/// `core.fileMode`: whether a file's executable bit is staged from disk.
	file_mode: bool,
	/// `core.symlinks`: whether a tracked symlink's plain-file placeholder is a file of its own.
	symlinks: bool,
}

impl<'a, F, W, H> Converter<'a, F, W, H>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	/// For staging, and for comparing working-tree files with `index`: `.gitattributes` from the
	/// working tree, `index`'s as the fallback. `index_time` is the index file's, as
	/// [`WorkTree::index_time`] read it before the index.
	pub(crate) async fn check_in(
		wt: &'a WorkTree<F, W, H>,
		index: &Index<H>,
		index_time: Option<SystemTime>,
	) -> Result<Self, WorktreeError> {
		let mut converter = Self::new(wt, attribute_entries(index), AttrDirection::CheckIn).await?;
		converter.index_time = index_time;
		Ok(converter)
	}

	/// For writing blobs out: `.gitattributes` from `indexed` (the entries of the index the checkout
	/// makes: path, mode and object), the working tree's as the fallback.
	pub(crate) async fn check_out<'p>(
		wt: &'a WorkTree<F, W, H>,
		indexed: impl IntoIterator<Item = (&'p GitPath, u32, ObjectId<H>)>,
	) -> Result<Self, WorktreeError> {
		Self::new(wt, indexed, AttrDirection::CheckOut).await
	}

	async fn new<'p>(
		wt: &'a WorkTree<F, W, H>,
		indexed: impl IntoIterator<Item = (&'p GitPath, u32, ObjectId<H>)>,
		direction: AttrDirection,
	) -> Result<Self, WorktreeError> {
		let attributes = Attributes::load(wt, indexed, direction).await?;
		let config = match wt.repository().effective_config().await {
			Ok(config) => config,
			// No config file is no settings, as git takes it.
			Err(RepositoryError::FileStore(FileStoreError::NotFound)) => GitConfig::default(),
			Err(error) => return Err(error.into()),
		};
		let eol = EolConfig::read(&config)?;
		let file_mode = crate::status::worktree_file_mode(wt).await;
		let symlinks = config
			.get_bool("core", None, "symlinks")
			.ok()
			.flatten()
			.unwrap_or(true);
		Ok(Converter {
			wt,
			attributes,
			direction,
			index_time: None,
			eol,
			config,
			warnings: Vec::new(),
			file_mode,
			symlinks,
		})
	}

	/// The mode to stage a regular file at `path` as, given the index's `existing` mode and the mode
	/// on disk (`actual`), as git's `ce_mode_from_stat`: under `core.symlinks=false` a tracked
	/// symlink's plain-file placeholder stays a symlink; under `core.fileMode=false` a tracked
	/// regular file keeps its executable bit, a new one `100644`.
	pub(crate) fn staged_file_mode(&self, existing: Option<u32>, actual: u32) -> u32 {
		match existing {
			Some(0o120000) if !self.symlinks => 0o120000,
			Some(mode @ (0o100644 | 0o100755)) if !self.file_mode => mode,
			_ if !self.file_mode => 0o100644,
			_ => actual,
		}
	}

	/// Stop reading the index's `.gitattributes` at `path`: one this operation stages as deleted, as
	/// git has already dropped it from its index when it converts the rest. Before any lookup.
	pub(crate) fn forget_indexed(&mut self, path: &GitPath) {
		self.attributes.forget_indexed(path);
	}

	/// Read the index view's `.gitattributes` at `path` as the target text of a symlink this operation
	/// stages there, as git reads it from its index. Before any lookup.
	pub(crate) fn stage_link(&mut self, path: &GitPath, target: Vec<u8>) {
		self.attributes.stage_link(path, target);
	}

	/// Whether gitana can convert the working-tree file `path` as its attributes ask: no filter
	/// driver, `ident` or `working-tree-encoding` it won't do.
	pub(crate) async fn can_convert(&mut self, path: &GitPath) -> Result<bool, WorktreeError> {
		Ok(self.attributes_of(path).await?.1.is_none())
	}

	/// Stop reading the working tree's `.gitattributes` at `path`: one this operation removes, which
	/// git has removed before it writes the rest. Before any lookup.
	pub(crate) fn forget_worktree(&mut self, path: &GitPath) {
		self.attributes.forget_worktree(path);
	}

	/// Whether `entry`'s stat, matching `meta`, vouches for the file (git's racy-timestamp rule
	/// against the index file's time).
	pub(crate) fn trusts_stat(&self, entry: &IndexEntry<H>, meta: &Meta) -> bool {
		crate::worktree::stat_trusted(entry, meta, self.index_time)
	}

	/// The working tree this converter is for.
	pub(crate) fn worktree(&self) -> &'a WorkTree<F, W, H> {
		self.wt
	}

	/// Note a staged file's line-ending warning, for [`Self::outcome`]; `tracked` when the index
	/// already had the file.
	pub(crate) fn note(&mut self, path: &GitPath, change: LineEndingChange, tracked: bool) {
		self.warnings.push((tracked, path.clone(), change));
	}

	/// What staging through this converter adds to `add`'s result: the warnings noted, in the order
	/// git's `add` gives them (it updates tracked files before adding new ones), each by path.
	pub(crate) fn outcome(mut self) -> crate::AddOutcome {
		self
			.warnings
			.sort_by(|(a_tracked, a, _), (b_tracked, b, _)| {
				b_tracked
					.cmp(a_tracked)
					.then_with(|| a.as_bytes().cmp(b.as_bytes()))
			});
		self.warnings.dedup_by(|(_, a, _), (_, b, _)| a == b);
		crate::AddOutcome {
			line_ending_warnings: self
				.warnings
				.into_iter()
				.map(|(_, path, change)| (path, change))
				.collect(),
		}
	}

	/// The attribute asking for a conversion gitana won't do, as written, if any: a filter whose
	/// driver would run this way (or one marked `required`, which git fails without a driver),
	/// `working-tree-encoding` other than UTF-8, or `ident`. In git's order: filter, encoding, ident.
	fn refusal(&self, attrs: &HashMap<Vec<u8>, AttrState>) -> Result<Option<String>, WorktreeError> {
		if let Some(AttrState::Value(name)) = attrs.get(b"filter".as_slice()) {
			let name = String::from_utf8_lossy(name).into_owned();
			let set = |key: &str| {
				self
					.config
					.get_string("filter", Some(&name), key)
					.is_some_and(|value| !value.is_empty())
			};
			let driver = set("process")
				|| match self.direction {
					AttrDirection::CheckIn => set("clean"),
					AttrDirection::CheckOut => set("smudge"),
				};
			let required = self.config.get_bool("filter", Some(&name), "required")? == Some(true);
			if driver || required {
				return Ok(Some(format!("filter={name}")));
			}
		}
		if let Some(AttrState::Value(encoding)) = attrs.get(b"working-tree-encoding".as_slice()) {
			let utf8 = encoding.eq_ignore_ascii_case(b"utf-8") || encoding.eq_ignore_ascii_case(b"utf8");
			if !encoding.is_empty() && !utf8 {
				return Ok(Some(format!(
					"working-tree-encoding={}",
					String::from_utf8_lossy(encoding)
				)));
			}
		}
		if attrs.get(b"ident".as_slice()) == Some(&AttrState::Set) {
			return Ok(Some("ident".to_owned()));
		}
		Ok(None)
	}

	/// Read every `filter.<name>.required` setting as a boolean, as git reads them all at its first
	/// conversion of any file: a bad one fails here. `mv` checks before its first rename, since
	/// git's own `mv` meets a bad value only after renaming (measured against git 2.55).
	pub(crate) fn check_filter_settings(&self) -> Result<(), WorktreeError> {
		for name in self.config.subsections("filter") {
			self.config.get_bool("filter", Some(name), "required")?;
		}
		Ok(())
	}

	/// The attributes of `path`, and the one asking for a conversion gitana won't do, if any.
	async fn attributes_of(
		&mut self,
		path: &GitPath,
	) -> Result<(HashMap<Vec<u8>, AttrState>, Option<String>), WorktreeError> {
		let attrs = self.attributes.of(self.wt, path).await?;
		let refusal = self.refusal(&attrs)?;
		Ok((attrs, refusal))
	}

	/// `content`, the working-tree file `path`, as the repository stores it. `indexed` is the index
	/// entry for the path, whose blob git's "auto" consults. `staging` applies `core.safecrlf`,
	/// as git's `add` does: `true` refuses an irreversible change.
	pub(crate) async fn convert_to_git<'c>(
		&mut self,
		path: &GitPath,
		content: &'c [u8],
		indexed: Option<&IndexEntry<H>>,
		staging: bool,
	) -> Result<Staged<'c>, WorktreeError> {
		let (attrs, refusal) = self.attributes_of(path).await?;
		if let Some(attribute) = refusal {
			return Ok(Staged::Unconvertible(attribute));
		}
		let action = crate::line_endings::crlf_action(&attrs, &self.eol);
		// A submodule's entry names a commit, not a blob: git's lookup counts it as no CRLF.
		let index_has_crlf = match indexed {
			Some(entry)
				if entry.mode != 0o160000 && crate::line_endings::needs_index_check(content, action) =>
			{
				crate::line_endings::blob_has_crlf(&self.wt.repository().read_blob(entry.oid).await?)
			}
			_ => false,
		};
		let safecrlf = staging.then_some(self.eol.safecrlf);
		match crate::line_endings::to_git(content, action, index_has_crlf, safecrlf) {
			Ok((stored, change)) => Ok(Staged::Content(stored, change)),
			Err(change) => Err(WorktreeError::IrreversibleLineEndings {
				path: path.clone(),
				change,
			}),
		}
	}

	/// `blob`, stored at `path`, as it's written into the working tree; refused if its
	/// attributes ask for a conversion gitana won't do.
	pub(crate) async fn convert_to_worktree<'c>(
		&mut self,
		path: &GitPath,
		blob: &'c [u8],
	) -> Result<Cow<'c, [u8]>, WorktreeError> {
		let (attrs, refusal) = self.attributes_of(path).await?;
		if let Some(attribute) = refusal {
			return Err(WorktreeError::UnsupportedAttribute {
				path: path.clone(),
				attribute,
			});
		}
		let action = crate::line_endings::crlf_action(&attrs, &self.eol);
		Ok(crate::line_endings::to_worktree(blob, action))
	}

	/// Refuse a blob of `mode` that would be written to `path` through a conversion gitana won't
	/// do, before anything is written. Only a regular file is converted; a symlink or a gitlink
	/// never is.
	pub(crate) async fn check_write(
		&mut self,
		path: &GitPath,
		mode: &str,
	) -> Result<(), WorktreeError> {
		if mode == "120000" || mode == "160000" {
			return Ok(());
		}
		match self.attributes_of(path).await?.1 {
			Some(attribute) => Err(WorktreeError::UnsupportedAttribute {
				path: path.clone(),
				attribute,
			}),
			None => Ok(()),
		}
	}

	/// What the working-tree entry `path` (of `meta`) holds, as the repository would store it: a
	/// symlink's target, or a file's content converted (as it is, when gitana can't convert it).
	/// `indexed` is the index entry for the path. `None` for anything else.
	pub(crate) async fn stored_form(
		&mut self,
		path: &GitPath,
		meta: &Meta,
		indexed: Option<&IndexEntry<H>>,
	) -> Result<Option<Vec<u8>>, WorktreeError> {
		if meta.kind.is_symlink() {
			return Ok(Some(self.wt.work().read_link(path)?));
		}
		if !meta.kind.is_file() {
			return Ok(None);
		}
		let content = self.wt.work().read(path)?;
		// The converted bytes when they're new; `None` when the content is stored as it is, or
		// gitana can't convert it.
		let converted = match self.convert_to_git(path, &content, indexed, false).await? {
			Staged::Content(Cow::Owned(stored), _) => Some(stored),
			Staged::Content(Cow::Borrowed(_), _) | Staged::Unconvertible(_) => None,
		};
		Ok(Some(converted.unwrap_or(content)))
	}

	/// The blob id and mode the working-tree entry `path` (of `meta`) would be staged as, to
	/// compare with `entry`, the index's: a symlink's target, a file's content as stored. A file
	/// gitana can't convert counts as unchanged while its stat matches `entry` (git's own
	/// shortcut), and otherwise gets its raw content's id, so it differs. `None` for anything else.
	pub(crate) async fn blob_of(
		&mut self,
		path: &GitPath,
		meta: &Meta,
		entry: Option<&IndexEntry<H>>,
	) -> Result<Option<(ObjectId<H>, u32)>, WorktreeError> {
		if let Some(entry) = entry
			&& meta.kind.is_file()
			&& self.trusts_stat(entry, meta)
			&& self.attributes_of(path).await?.1.is_some()
		{
			return Ok(Some((entry.oid, file_mode(meta))));
		}
		self.hashed_blob_of(path, meta, entry).await
	}

	/// [`Self::blob_of`] without trusting the stat: for the checks that decide whether a file may be
	/// deleted. A file gitana can't convert gets its raw content's id, so it differs from `entry`
	/// unless its bytes are the stored blob's.
	pub(crate) async fn hashed_blob_of(
		&mut self,
		path: &GitPath,
		meta: &Meta,
		entry: Option<&IndexEntry<H>>,
	) -> Result<Option<(ObjectId<H>, u32)>, WorktreeError> {
		let mode = if meta.kind.is_symlink() {
			0o120000
		} else {
			file_mode(meta)
		};
		Ok(
			self
				.stored_form(path, meta, entry)
				.await?
				.map(|stored| (ObjectId::<H>::compute(ObjectKind::Blob, &stored), mode)),
		)
	}
}

/// `index`'s entries as git reads attributes files from it: each path's stage 0, or, for a
/// `.gitattributes` with none (unmerged), its stage 2 (ours), as git's `read_blob_data_from_index`
/// falls back mid-merge.
pub(crate) fn attribute_entries<H: HashAlgorithm>(
	index: &Index<H>,
) -> impl Iterator<Item = (&GitPath, u32, ObjectId<H>)> {
	index
		.entries
		.iter()
		.filter(|entry| {
			entry.stage == 0
				|| (entry.stage == 2
					&& crate::attributes::attributes_folder(&entry.path).is_some()
					&& index.entry(&entry.path).is_none())
		})
		.map(|entry| (&entry.path, entry.mode, entry.oid))
}
