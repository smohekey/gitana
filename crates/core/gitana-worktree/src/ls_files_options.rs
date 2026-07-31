/// Which sets of paths `ls-files` lists, and how it renders them — the flag surface of git's
/// `ls-files`.
///
/// `cached` (the default, when no set is explicitly selected) lists index entries; `stage`
/// additionally switches index-backed lines to `<mode> <sha> <stage>\t<path>`. `others` lists
/// untracked working-tree files (ignored ones included, unless `exclude_standard`). `modified` /
/// `deleted` list the index entries whose working file differs from / is absent on disk.
/// `error_unmatch` reports when a pathspec matched nothing shown; `z` NUL-terminates each line and
/// disables path quoting; `full_name` prints repository-relative paths instead of cwd-relative.
#[derive(Debug, Clone, Default)]
pub struct LsFilesOptions {
	pub cached: bool,
	pub stage: bool,
	pub others: bool,
	pub modified: bool,
	pub deleted: bool,
	pub exclude_standard: bool,
	pub error_unmatch: bool,
	pub z: bool,
	pub full_name: bool,
}

impl LsFilesOptions {
	/// Whether the cached set is shown: explicitly (`-c`/`-s`), or by default when no selector at all
	/// (`-c`/`-s`/`-o`/`-m`/`-d`) is given — git's fallback to `--cached`.
	pub(crate) fn show_cached(&self) -> bool {
		self.cached || self.stage || !(self.others || self.modified || self.deleted)
	}
}
