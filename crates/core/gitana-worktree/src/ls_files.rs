//! `ls-files`: list index and/or working-tree paths, filtered by pathspec and rendered git's way —
//! cwd-relative output (with `--full-name` to opt back to repository-relative), C-style path quoting
//! (`core.quotePath`), and git's cached / others / modified / deleted selection. The selection sets
//! combine, and a path selected by several is emitted once per set (git's exact behaviour: a
//! conflicted path lists once per index stage, and `-c -m -d` prints a modified-and-deleted path
//! three times).

use std::collections::HashSet;

use gitana_file_store::FileStore;
use gitana_file_store_local::WorkDirFs;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind};
use gitana_path::{GitPath, GitPathComponent, GitPathspec};

use crate::fsmeta::{join_rel, push_gitignore};
use crate::ignore::{self, DirIgnore};
use crate::pathspec::PathspecSet;
use crate::status::worktree_change;
use crate::submodule_head_oid;
use crate::{IndexEntry, LsFilesOptions, WorkTree, WorktreeError};

/// The rendered output plus, when `--error-unmatch` is set, the first pathspec that matched nothing
/// shown. git prints the matched entries *and then* exits non-zero, so the caller writes `text`
/// verbatim before acting on `unmatched`.
pub struct LsFilesOutput {
	pub text: Vec<u8>,
	pub unmatched: Option<GitPathspec>,
}

/// Config the caller resolves from git's full stack, which the sandboxed worktree crate cannot reach
/// (global/system layers, and files outside the worktree). `quote_path` is `core.quotePath`;
/// `file_mode` is the effective `core.fileMode` (gates whether an exec-bit-only change counts as
/// modified for `-m`); `ignore_case` is `core.ignoreCase` (case-folds `--exclude-standard` matching);
/// `excludes_file` is the content of git's standard excludes file, consulted only for
/// `-o`/`--exclude-standard`.
pub struct LsFilesConfig<'a> {
	pub quote_path: bool,
	pub file_mode: bool,
	pub ignore_case: bool,
	/// The effective `core.symlinks`. When `false`, a `120000` symlink entry is materialised as a plain
	/// file holding the link target, and such a placeholder is *not* a modification for `-m`.
	pub symlinks: bool,
	pub excludes_file: Option<&'a [u8]>,
}

/// Run `ls-files` over `wt` with the caller-resolved `config`.
pub(crate) async fn run<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	pathspecs: &[GitPathspec],
	prefix: &GitPath,
	opts: &LsFilesOptions,
	config: &LsFilesConfig<'_>,
) -> Result<LsFilesOutput, WorktreeError> {
	let quote_path = config.quote_path;
	let excludes_file = config.excludes_file;
	let index = wt.load_index().await?;

	// The effective pathspec set. When it has no *positive* pathspec — none given, or only
	// `:(exclude)` ones — git still scopes the listing to the current subtree, so synthesize the
	// prefix directory as an implicit `.` positive (parsed at the same prefix as the rest).
	let mut set = PathspecSet::parse(pathspecs, prefix)?;
	if set.is_positive_empty() && !prefix.is_root() {
		let mut scoped = Vec::with_capacity(pathspecs.len() + 1);
		scoped.push(GitPathspec::from_utf8(".").expect("dot is a valid pathspec"));
		scoped.extend_from_slice(pathspecs);
		set = PathspecSet::parse(&scoped, prefix)?;
	}

	let term = if opts.z { b'\0' } else { b'\n' };
	// `--error-unmatch` with pathspecs that each name an *exact single file* collapses the output to one
	// line per path — dropping the per-selector duplicates and a conflicted path's extra stages, keeping
	// the first line emitted for each (probed vs git 2.50.1: `--error-unmatch del` prints `del` once, but a
	// directory pathspec, a glob, or `.` keeps the per-selector duplicates). git's fuller `--error-unmatch`
	// bookkeeping across selector passes — how a `:(exclude)` interacts with combined selectors, and its
	// asymmetric per-pathspec "matched" accounting (a positive matching a tracked-but-excluded path counts
	// as matched, but the same over an *untracked* excluded path does not) — is not reproduced. For those
	// exotic exclusion combinations both the duplicate-line output and, in the untracked case, the exit
	// status can differ from git; a deliberately documented divergence (see TODO.md / the initiative notes).
	let index_paths: HashSet<&GitPath> = index.entries.iter().map(|e| &e.path).collect();
	let dedup = opts.error_unmatch
		&& !pathspecs.is_empty()
		&& pathspecs
			.iter()
			.all(|spec| is_exact_file_pathspec(spec.as_bytes(), prefix, &index_paths));
	let mut seen: HashSet<GitPath> = HashSet::new();
	let mut out = Vec::new();
	let mut emit = |out: &mut Vec<u8>, path: &GitPath, line: Vec<u8>| {
		if dedup && !seen.insert(path.clone()) {
			return;
		}
		out.extend_from_slice(&line);
		out.push(term);
	};

	// Block 1 — others (untracked). git prints these first, as one sorted block, always plain (never
	// stage-formatted, even under `-s`).
	if opts.others {
		// Under `core.ignoreCase` git matches a working-tree entry to a tracked index path case-folded, so a
		// disk `foo` counts as the tracked `Foo` (untracked detection is ASCII-case-insensitive). Fold the
		// membership keys — and the lookups in `collect_others` — the same way.
		let fold_key = |path: &GitPath| {
			if config.ignore_case {
				path.ascii_folded()
			} else {
				path.as_bytes().to_vec()
			}
		};
		let tracked: HashSet<Vec<u8>> = index.entries.iter().map(|e| fold_key(&e.path)).collect();
		// Gitlink (submodule) paths — mode `160000`. git never lists a tracked gitlink directory under
		// `-o`; an ordinary tracked file whose path is now a directory (a file→dir replacement) is *not* a
		// gitlink, so it is still descended into.
		// Exclude a gitlink path that ALSO has tracked children (a mixed subtree-vs-gitlink conflict): the
		// on-disk directory holds tracked files, so `-o` descends into it and reports `sub/new`, as status
		// does — not an opaque submodule mount.
		// Fold-aware (matches `fold_key`): a mixed `160000 Sub` + `sub/f` conflict compares by fold-key.
		let has_tracked_child = |path: &GitPath| {
			let mut prefix = fold_key(path);
			prefix.push(b'/');
			index
				.entries
				.iter()
				.any(|e| fold_key(&e.path).starts_with(&prefix))
		};
		let gitlinks: HashSet<Vec<u8>> = index
			.entries
			.iter()
			.filter(|e| e.mode == 0o160000 && !has_tracked_child(&e.path))
			.map(|e| fold_key(&e.path))
			.collect();
		let mut stack: Vec<DirIgnore> = Vec::new();
		// `--exclude-standard` adds git's standard excludes below the per-directory `.gitignore` files:
		// the global excludes file (lowest priority) then `.git/info/exclude`, so a later per-directory
		// rule overrides them (git's precedence, evaluated last-match-wins in the stack).
		if opts.exclude_standard {
			if let Some(text) = excludes_file {
				stack.push(ignore::parse(text, b""));
			}
			if let Some(text) = crate::excludes::read_info_exclude(wt).await? {
				stack.push(ignore::parse(&text, b""));
			}
		}
		let mut others = Vec::new();
		let walk = OthersWalk {
			tracked: &tracked,
			gitlinks: &gitlinks,
			exclude_standard: opts.exclude_standard,
			ignore_case: config.ignore_case,
		};
		collect_others(wt.work(), &GitPath::root(), &walk, &mut stack, &mut others)?;
		// Git orders the names it serializes, so an opaque directory's presentation-only `/`
		// participates in the comparison (`foo.` sorts before `foo/`). Keep the domain path
		// canonical while comparing the raw presented byte streams without allocating.
		others.sort_by(|left, right| {
			left
				.path
				.as_bytes()
				.iter()
				.copied()
				.chain(left.directory.then_some(b'/'))
				.cmp(
					right
						.path
						.as_bytes()
						.iter()
						.copied()
						.chain(right.directory.then_some(b'/')),
				)
		});
		for entry in &others {
			if matches_presented_path(&set, &entry.path, entry.directory) {
				emit(
					&mut out,
					&entry.path,
					render(
						None::<&IndexEntry<H>>,
						&entry.path,
						entry.directory,
						prefix,
						opts,
						quote_path,
					),
				);
			}
		}
	}

	// Block 2 — cached / deleted / modified. Iterate the index once (git's stable path,stage order);
	// per entry emit a line for each selected set that applies, in git's sub-order (cached, deleted,
	// modified). A path is matched (for `--error-unmatch`) only when it is actually shown, so the
	// pathspec test is guarded behind each set's condition.
	if opts.show_cached() || opts.modified || opts.deleted {
		for entry in &index.entries {
			let directory = entry.mode & 0o170000 == 0o040000;
			if opts.show_cached() && matches_presented_path(&set, &entry.path, directory) {
				emit(
					&mut out,
					&entry.path,
					render(
						Some(entry),
						&entry.path,
						directory,
						prefix,
						opts,
						quote_path,
					),
				);
			}
			if opts.modified || opts.deleted {
				// git ignores the working tree for a skip-worktree (sparse) entry ENTIRELY: neither `-m` nor `-d`
				// inspects its file, present or absent — and this holds for a submodule (gitlink) too, even when
				// its mount is present and its `HEAD` has moved (probed vs git 2.55: `ls-files -m sub` is empty
				// after `update-index --skip-worktree sub`).
				if entry.skip_worktree {
					continue;
				}
				// Classify without over-reading: `-d` needs only an `lstat` (absence). Content is read only
				// for a present, `-m`-selected, non-assume-valid entry — and a read failure is treated as a
				// modification (git never aborts `ls-files` on an unreadable tracked file; assume-valid is
				// trusted and never re-examined). An `lstat` that itself fails (e.g. an unreadable parent
				// directory) counts as absent — git reports such a path under both `-m` and `-d`.
				let meta = wt.work().lstat(&entry.path).ok().flatten();
				let code = if meta.is_none() {
					'D'
				} else if !opts.modified || entry.assume_valid {
					' ' // present, and either `-d`-only or assume-valid → not a modification
				} else if entry.mode == 0o160000 {
					// A gitlink (submodule). A gitlink replaced on disk by a non-directory (a plain file or
					// symlink) is a modification. Otherwise git's `-m` reports it only when the checked-out
					// repository's `HEAD` commit differs from the recorded one (dirty state does not count); the
					// classifier can't hash a directory, so compare the submodule's `HEAD` to `entry.oid`
					// directly. An unresolvable submodule layout falls back to "unchanged" rather than a false
					// `M`.
					if !meta.as_ref().is_some_and(|meta| meta.kind.is_dir()) {
						'M'
					} else {
						match submodule_head_oid(wt, &entry.path).await {
							Some(head) if head != entry.oid => 'M',
							_ => ' ',
						}
					}
				} else if entry.mode == 0o120000
					&& !config.symlinks
					&& meta.as_ref().is_some_and(|meta| meta.kind.is_file())
				{
					// `core.symlinks=false`: a `120000` entry is materialised as a plain file holding the link
					// target. It is a modification only when that content no longer hashes to the recorded blob.
					if placeholder_matches(wt, entry) {
						' '
					} else {
						'M'
					}
				} else {
					// Present → `M` or ` `; a read failure (e.g. an unreadable file) counts as modified.
					worktree_change(wt.work(), entry, &entry.path, config.file_mode).unwrap_or('M')
				};
				// An absent file (`D`) is reported by both `-d` and `-m`. A present, diverged file (`M`) is
				// reported only by `-m` (assume-valid already collapsed to ` ` above).
				if code == 'D' {
					if opts.deleted && set.matches(&entry.path) {
						emit(
							&mut out,
							&entry.path,
							render(Some(entry), &entry.path, false, prefix, opts, quote_path),
						);
					}
					if opts.modified && set.matches(&entry.path) {
						emit(
							&mut out,
							&entry.path,
							render(Some(entry), &entry.path, false, prefix, opts, quote_path),
						);
					}
				} else if code == 'M' && opts.modified && set.matches(&entry.path) {
					emit(
						&mut out,
						&entry.path,
						render(Some(entry), &entry.path, false, prefix, opts, quote_path),
					);
				}
			}
		}
	}

	let unmatched = if opts.error_unmatch {
		match set.unmatched() {
			Some(spec) => Some(spec.clone()),
			// An exclusion-*only* pathspec (no positives) that shows nothing is still a failure: git checks
			// the implicit `.` scope and reports it unmatched (`ls-files -o --error-unmatch ':!u'` with `u`
			// the only untracked file exits non-zero). Gated on `is_positive_empty` so a matched-then-excluded
			// positive — `--error-unmatch a ':!a'`, which git exits 0 on — is not misreported.
			None if set.is_positive_empty() && out.is_empty() && !pathspecs.is_empty() => {
				Some(pathspecs.first().cloned().unwrap_or_else(|| {
					GitPathspec::from_utf8(".").expect("the implicit root pathspec is valid")
				}))
			}
			None => None,
		}
	} else {
		None
	};
	Ok(LsFilesOutput {
		text: out,
		unmatched,
	})
}

/// Match the pathname as it is presented by `ls-files`. Domain [`GitPath`] values stay canonical
/// and therefore never carry a trailing slash, while opaque working-tree directories and sparse
/// index directory entries are matched by their serialized `path/` spelling.
fn matches_presented_path(set: &PathspecSet, path: &GitPath, directory: bool) -> bool {
	if !directory {
		return set.matches(path);
	}
	let mut presented = Vec::with_capacity(path.as_bytes().len() + 1);
	presented.extend_from_slice(path.as_bytes());
	presented.push(b'/');
	set.matches(presented.as_slice())
}

/// Render one output entry: the `path` relativised and quoted per `opts`, prefixed with
/// `<mode> <sha> <stage>\t` when `opts.stage` and an index `entry` is supplied (others are always
/// plain, so they pass `None`).
fn render<H: HashAlgorithm>(
	entry: Option<&IndexEntry<H>>,
	path: &GitPath,
	directory: bool,
	prefix: &GitPath,
	opts: &LsFilesOptions,
	quote_path: bool,
) -> Vec<u8> {
	let mut rel = if opts.full_name {
		path.as_bytes().to_vec()
	} else {
		relativize(path, prefix)
	};
	if directory {
		rel.push(b'/');
	}
	let rendered = if opts.z {
		rel
	} else {
		quote_c_style(&rel, quote_path)
	};
	match entry {
		Some(entry) if opts.stage => {
			let mut line = format!(
				"{:06o} {} {}\t",
				entry.mode,
				entry.oid.to_hex(),
				entry.stage,
			)
			.into_bytes();
			line.extend_from_slice(&rendered);
			line
		}
		_ => rendered,
	}
}

/// The read-only context of the `-o` working-tree walk — everything that stays constant across the
/// recursion: the tracked / gitlink path sets (keys already folded per `core.ignoreCase`), whether
/// `--exclude-standard` is applying the ignore stack, and the `core.ignoreCase` fold flag.
struct OthersWalk<'a> {
	tracked: &'a HashSet<Vec<u8>>,
	gitlinks: &'a HashSet<Vec<u8>>,
	exclude_standard: bool,
	ignore_case: bool,
}

#[derive(Eq, PartialEq)]
struct OtherEntry {
	path: GitPath,
	directory: bool,
}

/// Recursively collect the working-tree files not present in the index (at any stage). With
/// `walk.exclude_standard`, apply the accumulated ignore rules per directory and skip ignored
/// entries; otherwise list ignored files too. Never collapses a directory — git's `-o` lists every
/// file individually — except an untracked embedded git repository, which git emits as the single
/// opaque directory entry (`inner/`) rather than descending into its contents.
fn collect_others<W: WorkDirFs>(
	work: &W,
	dir_rel: &GitPath,
	walk: &OthersWalk<'_>,
	stack: &mut Vec<DirIgnore>,
	out: &mut Vec<OtherEntry>,
) -> Result<(), WorktreeError> {
	// A directory we cannot read is skipped rather than fatal — git warns and continues (`ls-files -o`
	// stays exit 0 with a permission-denied directory present).
	let Ok(entries) = work.read_dir(dir_rel) else {
		return Ok(());
	};

	// An unusable per-directory `.gitignore` (permission-denied, or a directory at that path) is
	// non-fatal — git warns and continues — so a load failure just contributes no rules here.
	let pushed = walk.exclude_standard && push_gitignore(work, dir_rel, stack).unwrap_or(false);

	for entry in entries {
		if entry.name == ".git" {
			continue;
		}
		let rel = join_rel(dir_rel, &entry.name);
		// The membership key mirrors `core.ignoreCase`: ASCII-case-folded when set, so a disk `foo` matches
		// the tracked/gitlink `Foo`.
		let rel_key = if walk.ignore_case {
			rel.ascii_folded()
		} else {
			rel.as_bytes().to_vec()
		};
		// The kind is an `lstat`: a symlinked directory is a symlink (a file), not a directory.
		let is_dir = entry.kind.is_dir();
		if walk.exclude_standard && ignore::is_ignored_fold(&rel, is_dir, stack, walk.ignore_case) {
			continue;
		}
		if is_dir {
			// A tracked gitlink (submodule) directory is never listed under `-o`. An ordinary tracked path
			// that is now a directory (a file→dir replacement) is not a gitlink, so it is still descended.
			if walk.gitlinks.contains(&rel_key) {
				continue;
			}
			// An untracked *valid* embedded repository is opaque: git lists the single `dir/` entry and
			// never recurses. An empty or malformed `.git` is not a repository — git (and we) descend.
			if is_embedded_repo(work, &rel) {
				out.push(OtherEntry {
					path: rel,
					directory: true,
				});
			} else {
				collect_others(work, &rel, walk, stack, out)?;
			}
		} else if (entry.kind.is_file() || entry.kind.is_symlink()) && !walk.tracked.contains(&rel_key)
		{
			// git's `-o` lists regular files and symlinks; a socket / FIFO / device is never tracked and
			// never listed.
			out.push(OtherEntry {
				path: rel,
				directory: false,
			});
		}
	}

	if pushed {
		stack.pop();
	}
	Ok(())
}

/// Whether `dir` (worktree-relative) is a valid untracked embedded git repository — a `.git`
/// *directory* that is itself a git directory (holding `HEAD`, `objects`, and `refs`). git lists such
/// a directory opaquely under `-o`. An empty or malformed `.git`, or a `.git` gitfile (whose target
/// may lie outside the worktree capability), is not recognised here — git validates the marker, so an
/// unrecognised one is descended into like any ordinary directory.
pub(crate) fn is_embedded_repo<W: WorkDirFs>(work: &W, dir: &GitPath) -> bool {
	let git = dir.join(&GitPathComponent::from_utf8(".git").expect("valid marker"));
	// `.git` must be a directory; any `lstat` failure (e.g. an unreadable directory) leaves it
	// unrecognised — descended into, where an unreadable directory is then skipped by [`collect_others`].
	if !matches!(work.lstat(&git), Ok(Some(meta)) if meta.kind.is_dir()) {
		return false;
	}
	// `objects` and `refs` must be directories.
	for marker in ["objects", "refs"] {
		let marker = git.join(&GitPathComponent::from_utf8(marker).expect("valid marker"));
		if !matches!(work.lstat(&marker), Ok(Some(meta)) if meta.kind.is_dir()) {
			return false;
		}
	}
	// `HEAD` must be a file whose content is a valid ref (git's `validate_headref`): a `ref:` symref, or a
	// bare object id. `HEAD` containing garbage is not a repository — git descends.
	let head = git.join(&GitPathComponent::from_utf8("HEAD").expect("valid marker"));
	match work.read(&head) {
		Ok(bytes) => is_valid_head(&bytes),
		Err(_) => false,
	}
}

/// Whether `entry`'s working-tree file is a clean `core.symlinks=false` placeholder — a plain file
/// whose bytes hash to the recorded symlink-target blob (mode ignored).
fn placeholder_matches<F: FileStore, W: WorkDirFs, H: HashAlgorithm>(
	wt: &WorkTree<F, W, H>,
	entry: &IndexEntry<H>,
) -> bool {
	match wt.work().read(&entry.path) {
		Ok(bytes) => ObjectId::<H>::compute(ObjectKind::Blob, &bytes) == entry.oid,
		Err(_) => false,
	}
}

/// The commit id a checked-out submodule at `path` currently points at (its `HEAD`), or `None` when it
/// Whether `bytes` is a valid `HEAD`: a `ref:` symbolic ref pointing under `refs/`, or a bare object
/// id (40 hex for SHA-1, 64 for SHA-256). Mirrors git's `validate_headref` closely enough to tell a
/// real repository from a directory with a garbage `HEAD` — git rejects a symref target that is not a
/// `refs/…` name (e.g. `ref: nonsense`) and descends into the directory.
fn is_valid_head(bytes: &[u8]) -> bool {
	let Ok(text) = std::str::from_utf8(bytes) else {
		return false;
	};
	// Only *trailing* whitespace is stripped: git rejects a `HEAD` with leading whitespace before the
	// `ref:`/object id (so such a directory is descended into, not treated as a repository).
	let text = text.trim_end();
	if let Some(target) = text.strip_prefix("ref:") {
		return target.trim_start().starts_with("refs/");
	}
	matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `spec` names an *exact single file* among `index_paths` — a plain literal (or a
/// `:(literal)` one) that, resolved against the invocation `prefix`, equals an index entry and is not
/// the prefix of a deeper path (i.e. not a directory). git's `--error-unmatch` deduplicates output
/// only for such pathspecs (see [`run`]); a directory pathspec, a glob, or other magic keeps the
/// per-selector duplicates. The spec is normalised (prefix applied, `.`/`..` resolved) before the
/// comparison, since a pathspec is cwd-relative while index paths are repository-relative.
fn is_exact_file_pathspec(spec: &[u8], prefix: &GitPath, index_paths: &HashSet<&GitPath>) -> bool {
	// The literal path and the base it resolves against, for the magic forms that can still name an exact
	// file: plain and `:(literal)` are prefix-relative; top magic (`:/` / `:(top)`) is repository-root
	// relative. Any other magic (`:!`, `:^`, `:(glob)`, `:(icase)`, …) is not a plain exact-file spec.
	let (literal, base): (&[u8], &[u8]) = if let Some(rest) = spec.strip_prefix(b":(literal)") {
		(rest, prefix.as_bytes())
	} else if let Some(rest) = spec.strip_prefix(b":(top)") {
		(rest, b"")
	} else if let Some(rest) = spec.strip_prefix(b":/") {
		(rest, b"")
	} else if spec.starts_with(b":") {
		return false;
	} else {
		(spec, prefix.as_bytes())
	};
	if literal
		.iter()
		.any(|byte| matches!(byte, b'*' | b'?' | b'['))
	{
		return false; // a glob is not an exact file
	}
	let Ok((path, dir_only)) = crate::pathspec::normalize(literal, base) else {
		return false;
	};
	if dir_only {
		return false; // a trailing-slash / `.` spec is a directory, never an exact file
	}
	let Ok(path) = GitPath::from_bytes(path) else {
		return false;
	};
	index_paths.contains(&path) && !index_paths.iter().any(|entry| entry.is_below(&path))
}

/// Render `path` (worktree-relative, `/`-joined) relative to the `prefix` directory — git's
/// cwd-relative output. A path under the prefix has it stripped; one outside gets `../` segments.
/// An empty prefix returns `path` unchanged.
fn relativize(path: &GitPath, prefix: &GitPath) -> Vec<u8> {
	if prefix.is_root() {
		return path.as_bytes().to_vec();
	}
	let prefix_parts: Vec<&[u8]> = prefix.components().collect();
	let path_parts: Vec<&[u8]> = path.components().collect();
	let common = prefix_parts
		.iter()
		.zip(&path_parts)
		.take_while(|(a, b)| a == b)
		.count();
	let mut out = Vec::new();
	for _ in 0..prefix_parts.len() - common {
		out.extend_from_slice(b"../");
	}
	for (index, part) in path_parts[common..].iter().enumerate() {
		if index > 0 {
			out.push(b'/');
		}
		out.extend_from_slice(part);
	}
	out
}

/// C-style quote `s` the way git does for path output. Returns `s` unchanged when nothing needs
/// escaping; otherwise wraps it in double quotes, escaping backslash, double-quote, the named
/// control escapes (`\a \b \t \n \v \f \r`), other control bytes and DEL as octal `\NNN`, and — when
/// `quote_path` (`core.quotePath`) is set — every byte of a non-ASCII character as octal too.
fn quote_c_style(s: &[u8], quote_path: bool) -> Vec<u8> {
	if !s.iter().copied().any(|byte| needs_quote(byte, quote_path)) {
		return s.to_vec();
	}
	let mut out = Vec::with_capacity(s.len() + 2);
	out.push(b'"');
	for &byte in s {
		if byte.is_ascii() {
			match named_escape(byte) {
				Some(escape) => {
					out.push(b'\\');
					out.push(escape as u8);
				}
				None if byte < 0x20 || byte == 0x7f => push_octal_bytes(&mut out, byte),
				None => out.push(byte),
			}
		} else if quote_path {
			push_octal_bytes(&mut out, byte);
		} else {
			out.push(byte);
		}
	}
	out.push(b'"');
	out
}

/// Whether `c` would be escaped by [`quote_c_style`] (so the whole string must be quoted).
fn needs_quote(byte: u8, quote_path: bool) -> bool {
	if byte.is_ascii() {
		named_escape(byte).is_some() || byte < 0x20 || byte == 0x7f
	} else {
		quote_path
	}
}

fn push_octal_bytes(out: &mut Vec<u8>, byte: u8) {
	out.push(b'\\');
	out.push(b'0' + ((byte >> 6) & 0x07));
	out.push(b'0' + ((byte >> 3) & 0x07));
	out.push(b'0' + (byte & 0x07));
}

/// git's single-letter C escape for a byte (`\a \b \t \n \v \f \r \" \\`), or `None`.
fn named_escape(b: u8) -> Option<char> {
	match b {
		0x07 => Some('a'),
		0x08 => Some('b'),
		0x09 => Some('t'),
		0x0a => Some('n'),
		0x0b => Some('v'),
		0x0c => Some('f'),
		0x0d => Some('r'),
		b'"' => Some('"'),
		b'\\' => Some('\\'),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use gitana_path::GitPath;

	use super::{quote_c_style, relativize};

	#[test]
	fn relativize_strips_and_ascends() {
		let path = |value| GitPath::from_utf8(value).unwrap();
		assert_eq!(relativize(&path("src/lib.rs"), &path("src")), b"lib.rs");
		assert_eq!(relativize(&path("src/lib.rs"), &path("src")), b"lib.rs");
		assert_eq!(
			relativize(&path("vendor/x.rs"), &path("src")),
			b"../vendor/x.rs"
		);
		assert_eq!(relativize(&path("a/b/c.rs"), &path("a/x")), b"../b/c.rs");
		assert_eq!(
			relativize(&path("README.md"), &GitPath::root()),
			b"README.md"
		);
	}

	/// C-style quoting matches git: no quoting when unnecessary, named escapes, octal for control and
	/// (under `core.quotePath`) non-ASCII bytes, and raw non-ASCII when `quotePath` is off.
	#[test]
	fn quote_c_style_matches_git() {
		assert_eq!(quote_c_style(b"plain.txt", true), b"plain.txt");
		assert_eq!(quote_c_style(b"back\\slash", true), b"\"back\\\\slash\"");
		assert_eq!(quote_c_style(b"quo\"te", true), b"\"quo\\\"te\"");
		assert_eq!(quote_c_style(b"tab\tfile", true), b"\"tab\\tfile\"");
		assert_eq!(quote_c_style(b"line\nfeed", true), b"\"line\\nfeed\"");
		// A bell (0x07) uses the named `\a`; a vertical tab (0x0b) uses `\v`.
		assert_eq!(quote_c_style(b"a\x07b", true), b"\"a\\ab\"");
		// DEL (0x7f) and other unnamed control bytes are octal.
		assert_eq!(quote_c_style(b"x\x7fy", true), b"\"x\\177y\"");
		// "café" — the é is UTF-8 c3 a9: octal-escaped per byte with quotePath, literal without.
		assert_eq!(quote_c_style("café".as_bytes(), true), b"\"caf\\303\\251\"");
		assert_eq!(quote_c_style("café".as_bytes(), false), "café".as_bytes());
	}
}
