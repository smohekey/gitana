//! Sparse-checkout pattern matching (`.git/info/sparse-checkout`).
//!
//! A sparse checkout omits part of the working tree: a path that the patterns do *not* include gets the
//! **skip-worktree** index bit (removed from the work tree, kept in the index). git has two pattern
//! modes, selected by `core.sparseCheckoutCone`:
//!
//! - **cone** (git's default, this module) — a set of directories. A file is included iff it is a
//!   root-level file, a file directly in an *ancestor* of an included directory, or anything under an
//!   included directory. git encodes this in `.git/info/sparse-checkout` as `/*` + `!/*/` (root files)
//!   plus, for each included directory `D`, its ancestors' `/A/` + `!/A/*/` (A's own files, not its
//!   other subdirs) and the leaf's `/D/` (D recursively). This is a directory-prefix model, *not* a
//!   literal evaluation of those glob lines (a file `a/f` is included under `set a/b`, yet the literal
//!   dir-only pattern `/a/` would not match a file).
//! - **non-cone** — full gitignore-style patterns evaluated hierarchically. A later addition (it reuses
//!   [`crate::ignore`]'s glob engine but needs git's per-directory descent, not the ignore stack).

use std::collections::BTreeSet;

use crate::ignore::{self, DirIgnore};

/// The outcome of a sparse-checkout reapply. Both lists are tracked paths git reports and the user
/// resolves, then re-runs reapply — no work is lost in either case:
///
/// - `left_dirty`: paths the reapply *would* have omitted (outside the sparse patterns) but left in
///   the working tree because they had local modifications — their skip-worktree bit was NOT set. git
///   prints "not up to date and were left despite sparse patterns".
/// - `not_updated`: included paths that could *not* be materialised because an untracked file occupies
///   an ancestor slot (materialising would have had to delete it). git preserves that file, clears the
///   path's skip-worktree bit but writes nothing, and prints "already present and thus not updated
///   despite sparse patterns" (leaving the path showing as deleted in `status`).
#[derive(Debug, Default)]
pub struct SparseReapply {
	pub left_dirty: Vec<String>,
	pub not_updated: Vec<String>,
}

/// The user-facing sparse-checkout set — the arguments to `sparse-checkout set`/`add` and what `list`
/// prints — in whichever mode is (or is being) configured. `Cone` carries the included directories;
/// `NonCone` carries full gitignore-style pattern lines. This is the *semantic* set; persisting it
/// renders the on-disk `.git/info/sparse-checkout` (cone rounds through [`Cone`], non-cone is verbatim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SparseSet {
	Cone(Vec<String>),
	NonCone(Vec<String>),
}

impl SparseSet {
	/// Whether this set is cone-mode (`core.sparseCheckoutCone`).
	pub fn is_cone(&self) -> bool {
		matches!(self, Self::Cone(_))
	}

	/// The set's entries — included directories (cone) or pattern lines (non-cone).
	pub fn entries(&self) -> &[String] {
		match self {
			Self::Cone(dirs) => dirs,
			Self::NonCone(lines) => lines,
		}
	}

	/// Render to the exact `.git/info/sparse-checkout` bytes git writes for this set: cone rounds through
	/// [`Cone`] (git's `/*` + `!/*/` + per-dir shape), non-cone is the pattern lines verbatim, each
	/// newline-terminated.
	pub(crate) fn render(&self) -> String {
		match self {
			Self::Cone(dirs) => Cone::from_dirs(dirs.iter().cloned()).render(),
			Self::NonCone(lines) => {
				let mut out = String::new();
				for line in lines {
					out.push_str(line);
					out.push('\n');
				}
				out
			}
		}
	}
}

/// The active sparse-checkout matcher, in whichever mode `core.sparseCheckoutCone` selects. A file is
/// materialised (present in the working tree) iff [`includes`](Self::includes); otherwise its index
/// entry gets the skip-worktree bit.
pub(crate) enum SparseCheckout {
	Cone(Cone),
	NonCone(NonCone),
}

impl SparseCheckout {
	/// Parse `.git/info/sparse-checkout` text under the configured mode (`cone` = `core.sparseCheckoutCone`).
	/// In cone mode, if the file carries a line that is not a valid cone pattern — e.g. a hand-edited
	/// `/x/*.txt` — git warns and falls back to ordinary non-cone matching for the whole file (`reapply`
	/// supports manual edits), so a non-cone line is honoured rather than silently dropped.
	pub(crate) fn parse(text: &str, cone: bool, ignorecase: bool) -> Self {
		if cone && is_cone_compatible(text) {
			Self::Cone(Cone::parse(text, ignorecase))
		} else {
			Self::NonCone(NonCone::parse(text, ignorecase))
		}
	}

	/// Whether the file at `path` (root-relative, slash-separated) is in the sparse checkout.
	pub(crate) fn includes(&self, path: &str) -> bool {
		match self {
			Self::Cone(cone) => cone.includes(path),
			Self::NonCone(non_cone) => non_cone.includes(path),
		}
	}
}

/// Whether the sparse-checkout file is a valid cone file (so cone matching applies): every line is
/// `/*`, `!/*/`, a directory `/D/`, or a parent `!/D/*/` (with `D` free of glob metacharacters and
/// backslashes), AND every negative parent `!/D/*/` has its corresponding `/D/`. git falls back to
/// non-cone matching for the whole file when any line is not cone-shaped, or when a negative parent is
/// orphaned — it reports "unrecognized negative pattern" (probed vs git 2.50.1). In non-cone the escape
/// or glob is then honoured (e.g. `/a\b/` includes directory `ab`).
fn is_cone_compatible(text: &str) -> bool {
	let mut included: BTreeSet<&str> = BTreeSet::new();
	let mut negative_parents: Vec<&str> = Vec::new();
	for raw in text.lines() {
		let line = raw.trim();
		if line.is_empty() || line.starts_with('#') || line == "/*" || line == "!/*/" {
			continue;
		}
		if let Some(inner) = line
			.strip_prefix("!/")
			.and_then(|rest| rest.strip_suffix("/*/"))
		{
			if !is_cone_dir_name(inner) {
				return false;
			}
			negative_parents.push(inner);
		} else if let Some(inner) = line
			.strip_prefix('/')
			.and_then(|rest| rest.strip_suffix('/'))
		{
			if !is_cone_dir_name(inner) {
				return false;
			}
			included.insert(inner);
		} else {
			return false;
		}
	}
	// An orphan `!/D/*/` (a negative parent without its `/D/`) is an unrecognized negative pattern in
	// git's cone parser — the whole file falls back to non-cone matching.
	negative_parents.iter().all(|dir| included.contains(dir))
}

/// Whether `D` is a valid cone directory name: non-empty and free of glob metacharacters (`*`/`?`/`[`/
/// `]`) and backslashes. A `\` is a gitignore escape, not a literal, so git treats a cone line carrying
/// one as non-cone-shaped (probed vs git 2.50.1) — its presence forces the whole-file non-cone fallback.
fn is_cone_dir_name(inner: &str) -> bool {
	!inner.is_empty() && !inner.contains(['*', '?', '[', ']', '\\'])
}

/// Non-cone (full gitignore-pattern) sparse-checkout patterns. Reuses [`crate::ignore`]'s pattern
/// parser and glob engine via [`ignore::sparse_match`] — the on-disk syntax is identical to
/// `.gitignore`; only the matching *semantics* differ (a match means included, a directory match
/// recurses into its subtree, and the last matching pattern wins).
pub(crate) struct NonCone {
	patterns: DirIgnore,
	ignorecase: bool,
}

impl NonCone {
	pub(crate) fn parse(text: &str, ignorecase: bool) -> Self {
		Self {
			patterns: ignore::parse(text, ""),
			ignorecase,
		}
	}

	pub(crate) fn includes(&self, path: &str) -> bool {
		ignore::sparse_match(path, &self.patterns, self.ignorecase)
	}
}

/// Parsed cone-mode sparse-checkout patterns: the directories whose subtree is fully included
/// (`recursive`, the args to `set`/`add`) and their ancestor directories whose *direct* files are
/// included (`parents`, always containing the root `""`). All normalised to slash-separated,
/// leading/trailing-slash-free paths (root = `""`).
pub(crate) struct Cone {
	recursive: BTreeSet<String>,
	parents: BTreeSet<String>,
	/// Fold ASCII case when matching (git's `core.ignoreCase`). Only affects [`includes`](Self::includes);
	/// the stored directories keep their original case for [`render`](Self::render)/[`dirs`](Self::dirs),
	/// which git preserves verbatim in the pattern file and `list` output.
	ignorecase: bool,
}

impl Cone {
	/// Build from the included (recursive) directories — the `set`/`add` arguments. Each is normalised
	/// (leading/trailing slashes stripped); a directory nested under another included one is dropped as
	/// redundant; the ancestor `parents` set is derived (root always included).
	pub(crate) fn from_dirs(dirs: impl IntoIterator<Item = String>) -> Self {
		let mut recursive: BTreeSet<String> = dirs
			.into_iter()
			.map(|d| normalize_dir(&d))
			.filter(|d| !d.is_empty())
			.collect();
		// Drop any included dir that is nested under another included dir (git normalises these away).
		let redundant: Vec<String> = recursive
			.iter()
			.filter(|d| {
				recursive
					.iter()
					.any(|o| o.as_str() != d.as_str() && is_under_fold(d, o, false))
			})
			.cloned()
			.collect();
		for d in redundant {
			recursive.remove(&d);
		}
		let mut parents = BTreeSet::new();
		parents.insert(String::new());
		for r in &recursive {
			for anc in ancestors(r) {
				parents.insert(anc);
			}
		}
		// A leaf that is also some other leaf's ancestor stays recursive (the wider inclusion wins).
		for r in &recursive {
			parents.remove(r);
		}
		// `from_dirs` builds the render/list surface, where case is preserved verbatim; matching (which
		// `ignorecase` governs) goes through `parse`. Default off here — the field is unused by `render`.
		Self {
			recursive,
			parents,
			ignorecase: false,
		}
	}

	/// Parse a cone-mode `.git/info/sparse-checkout`. Recognises the `/*`, `!/*/`, `/D/`, `!/D/*/` shapes
	/// git writes: a `/D/` with a matching `!/D/*/` is a parent (direct files only), one without is
	/// recursive (leaf). Lines that do not fit the cone shape are ignored (git, with cone configured,
	/// likewise treats the file as cone).
	pub(crate) fn parse(text: &str, ignorecase: bool) -> Self {
		let mut included = BTreeSet::new();
		let mut parents = BTreeSet::new();
		parents.insert(String::new());
		let mut saw_root_star = false;
		let mut saw_root_exclude = false;
		for raw in text.lines() {
			let line = raw.trim();
			if line.is_empty() || line.starts_with('#') {
				continue;
			}
			if line == "/*" {
				saw_root_star = true;
			} else if line == "!/*/" {
				saw_root_exclude = true;
			} else if let Some(inner) = line.strip_prefix("!/").and_then(|s| s.strip_suffix("/*/")) {
				parents.insert(inner.to_owned());
			} else if let Some(inner) = line.strip_prefix('/').and_then(|s| s.strip_suffix('/')) {
				included.insert(inner.to_owned());
			}
		}
		let mut recursive: BTreeSet<String> = included.difference(&parents).cloned().collect();
		// `/*` without `!/*/` includes the whole tree: git treats the root as recursive, not root-files-
		// only (probed vs git 2.50.1 for a hand-edited cone file). A recursive root ("") includes every
		// path. The standard cone default (`/*` + `!/*/`) keeps the root a parent — root files only.
		if saw_root_star && !saw_root_exclude {
			recursive.insert(String::new());
		}
		Self {
			recursive,
			parents,
			ignorecase,
		}
	}

	/// The included directories (the recursive leaves), sorted — what `sparse-checkout list` prints in
	/// cone mode (git recovers the dirs from the patterns rather than echoing the raw file).
	pub(crate) fn dirs(&self) -> Vec<String> {
		self.recursive.iter().cloned().collect()
	}

	/// Render to the exact byte format git writes: `/*`, `!/*/`, then each parent (sorted) as `/P/`,
	/// `!/P/*/`, then each recursive leaf (sorted) as `/L/`.
	pub(crate) fn render(&self) -> String {
		let mut out = String::from("/*\n!/*/\n");
		for p in &self.parents {
			if p.is_empty() {
				continue;
			}
			out.push('/');
			out.push_str(p);
			out.push_str("/\n!/");
			out.push_str(p);
			out.push_str("/*/\n");
		}
		for r in &self.recursive {
			out.push('/');
			out.push_str(r);
			out.push_str("/\n");
		}
		out
	}

	/// Whether the **file** at `path` (root-relative, slash-separated) is in the sparse checkout: a
	/// direct file of a parent directory (including the root), or anything under a recursive directory.
	pub(crate) fn includes(&self, path: &str) -> bool {
		let dir = match path.rfind('/') {
			Some(i) => &path[..i],
			None => "",
		};
		self.parent_contains(dir) || self.under_recursive(dir)
	}

	/// Whether `dir` is one of the parent directories (direct files included), folding case when
	/// `ignorecase`.
	fn parent_contains(&self, dir: &str) -> bool {
		if self.ignorecase {
			self.parents.iter().any(|p| p.eq_ignore_ascii_case(dir))
		} else {
			self.parents.contains(dir)
		}
	}

	/// Whether `dir` is at or below one of the recursive directories, folding case when `ignorecase`.
	fn under_recursive(&self, dir: &str) -> bool {
		self
			.recursive
			.iter()
			.any(|r| eq_fold(dir, r, self.ignorecase) || is_under_fold(dir, r, self.ignorecase))
	}
}

/// Normalise a directory argument to a cone key: strip surrounding whitespace and leading/trailing
/// slashes, so `"/a/b/"`, `"a/b"`, and `"a/b/"` all become `"a/b"` (root/`"."` become `""`).
fn normalize_dir(dir: &str) -> String {
	let d = dir.trim().trim_matches('/');
	if d == "." {
		String::new()
	} else {
		d.to_owned()
	}
}

/// Whether two directory keys are equal, folding ASCII case when `fold` (git's `core.ignoreCase`).
fn eq_fold(a: &str, b: &str, fold: bool) -> bool {
	if fold {
		a.eq_ignore_ascii_case(b)
	} else {
		a == b
	}
}

/// Whether `path` is strictly under directory `base` (`a/b` is under `a`, not under `ab`), folding
/// ASCII case when `fold`. The prefix comparison runs on bytes — the `/` separator at `base.len()` is
/// ASCII, so the guarded index is always a char boundary.
fn is_under_fold(path: &str, base: &str, fold: bool) -> bool {
	if base.is_empty() {
		return true;
	}
	path.len() > base.len()
		&& path.as_bytes()[base.len()] == b'/'
		&& if fold {
			path.as_bytes()[..base.len()].eq_ignore_ascii_case(base.as_bytes())
		} else {
			path.starts_with(base)
		}
}

/// The proper ancestor directories of `dir`, excluding the root (`""`) and `dir` itself:
/// `"a/b/c"` → `["a", "a/b"]`.
fn ancestors(dir: &str) -> Vec<String> {
	let mut out = Vec::new();
	let mut idx = 0;
	while let Some(next) = dir[idx..].find('/') {
		idx += next;
		out.push(dir[..idx].to_owned());
		idx += 1;
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn renders_the_git_cone_format() {
		// `set a/b` → root markers, the ancestor `a`, then the leaf `a/b` (probed against git 2.50.1).
		let cone = Cone::from_dirs(["a/b".to_owned()]);
		assert_eq!(cone.render(), "/*\n!/*/\n/a/\n!/a/*/\n/a/b/\n");

		// Two leaves: all parents (sorted) first, then all leaves (sorted) — git's grouping.
		let cone = Cone::from_dirs(["a/b".to_owned(), "x/y".to_owned()]);
		assert_eq!(
			cone.render(),
			"/*\n!/*/\n/a/\n!/a/*/\n/x/\n!/x/*/\n/a/b/\n/x/y/\n"
		);

		// A deeper leaf emits each ancestor as a parent.
		let cone = Cone::from_dirs(["a/b/c".to_owned()]);
		assert_eq!(
			cone.render(),
			"/*\n!/*/\n/a/\n!/a/*/\n/a/b/\n!/a/b/*/\n/a/b/c/\n"
		);
	}

	#[test]
	fn normalises_and_dedupes_included_dirs() {
		// Slashes stripped; a dir nested under another included dir dropped as redundant.
		let cone = Cone::from_dirs(["/a/".to_owned(), "a/b".to_owned()]);
		assert_eq!(cone.dirs(), vec!["a".to_owned()]);
		assert_eq!(cone.render(), "/*\n!/*/\n/a/\n");
	}

	#[test]
	fn cone_mode_falls_back_to_non_cone_for_a_non_cone_line() {
		// A hand-edited cone file with a glob line is not cone-shaped, so git (and now this) matches the
		// whole file as non-cone: `/x/*.txt` includes `x/t.txt` rather than dropping the line.
		let matcher = SparseCheckout::parse("/*\n!/*/\n/x/*.txt\n", true, false);
		assert!(matches!(matcher, SparseCheckout::NonCone(_)));
		assert!(matcher.includes("x/t.txt"));
		// A purely cone-shaped file still uses cone matching.
		assert!(matches!(
			SparseCheckout::parse("/*\n!/*/\n/a/\n", true, false),
			SparseCheckout::Cone(_)
		));
	}

	#[test]
	fn cone_mode_falls_back_for_an_orphan_negative_parent() {
		// `!/a/*/` without its `/a/` is an unrecognized negative pattern in git's cone parser: the file
		// matches as non-cone, excluding the directory `a` (probed vs git 2.50.1) rather than including its
		// direct files as a cone parent would.
		let matcher = SparseCheckout::parse("/*\n!/*/\n!/a/*/\n", true, false);
		assert!(matches!(matcher, SparseCheckout::NonCone(_)));
		assert!(!matcher.includes("a/f"));
		assert!(matcher.includes("root.txt"));
	}

	#[test]
	fn cone_lone_root_star_includes_the_whole_tree() {
		// A hand-edited cone file with only `/*` (no `!/*/`) includes the entire tree — git treats `/*` as
		// a recursive root (probed vs git 2.50.1), not root-files-only.
		let cone = Cone::parse("/*\n", false);
		assert!(cone.includes("root.txt"));
		assert!(cone.includes("a/f"));
		assert!(cone.includes("a/b/deep"));
		// The standard default (`/*` + `!/*/`) is still root-files-only.
		let default = Cone::parse("/*\n!/*/\n", false);
		assert!(default.includes("root.txt"));
		assert!(!default.includes("a/f"));
	}

	#[test]
	fn cone_mode_falls_back_to_non_cone_for_a_backslash_line() {
		// A hand-edited cone line with a backslash is not cone-shaped (git warns and disables cone). The
		// file matches as non-cone, honouring the escape: `/a\b/` includes directory `ab` (probed vs git
		// 2.50.1) — not a literal cone directory `a\b` that would exclude `ab`.
		let matcher = SparseCheckout::parse("/*\n!/*/\n/a\\b/\n", true, false);
		assert!(matches!(matcher, SparseCheckout::NonCone(_)));
		assert!(matcher.includes("ab/f"));
	}

	#[test]
	fn non_cone_unescapes_leading_markers() {
		// `\!keep` is the literal file `!keep` (not a negation); `\#hash` the file `#hash` (not a comment).
		let matcher = SparseCheckout::parse("\\!keep\n\\#hash\n", false, false);
		assert!(matcher.includes("!keep"));
		assert!(matcher.includes("#hash"));
	}

	#[test]
	fn includes_files_by_cone_rule() {
		// `set a/b`: root files in; a's own files in (ancestor parent); everything under a/b in; a's
		// other subdirs out; unrelated dirs out. (Mirrors the probed `ls-files -t`.)
		let cone = Cone::from_dirs(["a/b".to_owned()]);
		assert!(cone.includes("root"));
		assert!(cone.includes("a/f"));
		assert!(cone.includes("a/b/f"));
		assert!(cone.includes("a/b/c/deep"));
		assert!(!cone.includes("a/other/f"));
		assert!(!cone.includes("x/f"));
	}

	#[test]
	fn cone_folds_case_under_ignorecase() {
		// `core.ignoreCase` on: a `Dir` pattern matches a `dir/f` index path (and its subtree),
		// case-insensitively — probed against git 2.50.1. Off: the mismatched case excludes it. The stored
		// directory keeps its original case for `render`/`dirs`.
		let text = "/*\n!/*/\n/Dir/\n";
		let folded = SparseCheckout::parse(text, true, true);
		assert!(folded.includes("dir/f"));
		assert!(folded.includes("DIR/sub/g"));
		let exact = SparseCheckout::parse(text, true, false);
		assert!(!exact.includes("dir/f"));
		assert!(exact.includes("Dir/f"));
		// A parent (direct-file) level folds too: `set a/b` includes `A/f` under ignorecase.
		let parent = SparseCheckout::parse("/*\n!/*/\n/a/\n!/a/*/\n/a/b/\n", true, true);
		assert!(parent.includes("A/f"));
		assert!(parent.includes("A/B/deep"));
	}

	#[test]
	fn non_cone_supports_posix_classes() {
		// `/file[[:digit:]].txt` includes file1.txt via git's wildmatch POSIX class, not fileA.txt
		// (probed vs git 2.50.1).
		let m = non_cone("/file[[:digit:]].txt\n");
		assert!(m.includes("file1.txt"));
		assert!(!m.includes("fileA.txt"));
	}

	#[test]
	fn non_cone_honours_backslash_escapes() {
		// An escaped metacharacter is literal: `/a\*` includes the file `a*`, not `ax` (git's gitignore
		// escapes, which non-cone sparse shares — probed vs git 2.50.1).
		let m = non_cone("/a\\*\n");
		assert!(m.includes("a*"));
		assert!(!m.includes("ax"));
	}

	#[test]
	fn non_cone_folds_case_under_ignorecase() {
		// Non-cone patterns fold too (git's core.ignoreCase, probed): `/Dir/` includes `dir/f`.
		let folded = NonCone::parse("/*\n!/*/\n/Dir/\n", true);
		assert!(folded.includes("dir/f"));
		let exact = NonCone::parse("/*\n!/*/\n/Dir/\n", false);
		assert!(!exact.includes("dir/f"));
	}

	#[test]
	fn parse_round_trips_render() {
		let cone = Cone::from_dirs(["a/b".to_owned(), "x/y".to_owned()]);
		let reparsed = Cone::parse(&cone.render(), false);
		assert_eq!(reparsed.dirs(), vec!["a/b".to_owned(), "x/y".to_owned()]);
		// And the reparsed matcher includes the same files.
		assert!(reparsed.includes("a/f"));
		assert!(reparsed.includes("x/y/deep/f"));
		assert!(!reparsed.includes("a/other/f"));
	}

	// --- non-cone: every case here was derived by probing git 2.50.1 (`ls-files -t`). ---

	fn non_cone(text: &str) -> NonCone {
		NonCone::parse(text, false)
	}

	#[test]
	fn non_cone_directory_pattern_includes_the_subtree() {
		// `/foo/` and unanchored `foo` both include foo and everything under it (recursive), nothing else.
		for pat in ["/foo/\n", "foo\n"] {
			let m = non_cone(pat);
			assert!(m.includes("foo/f.txt"), "{pat:?}");
			assert!(m.includes("foo/sub/h.txt"), "{pat:?}");
			assert!(!m.includes("root.txt"), "{pat:?}");
			assert!(!m.includes("bar/i.txt"), "{pat:?}");
		}
		// A deep anchored dir includes only its own subtree.
		let m = non_cone("/bar/deep/\n");
		assert!(m.includes("bar/deep/j.txt"));
		assert!(!m.includes("bar/i.txt"));
	}

	#[test]
	fn non_cone_glob_matches_basename_at_any_depth() {
		let m = non_cone("*.log\n");
		assert!(m.includes("a.log"));
		assert!(m.includes("foo/g.log"));
		assert!(!m.includes("foo/f.txt"));
	}

	#[test]
	fn non_cone_star_slash_includes_everything() {
		// `/*` matches each top-level entry; a directory match recurses — so it includes all files.
		let m = non_cone("/*\n");
		assert!(m.includes("root.txt"));
		assert!(m.includes("foo/f.txt"));
		assert!(m.includes("bar/deep/j.txt"));
	}

	#[test]
	fn non_cone_last_matching_pattern_wins() {
		// Include a dir, then exclude a subdir of it.
		let m = non_cone("/foo/\n!/foo/sub/\n");
		assert!(m.includes("foo/f.txt"));
		assert!(!m.includes("foo/sub/h.txt"));

		// Include by glob, then exclude one specific file.
		let m = non_cone("*.log\n!foo/g.log\n");
		assert!(m.includes("a.log"));
		assert!(!m.includes("foo/g.log"));

		// Order matters: a later include re-adds an earlier exclusion.
		assert!(non_cone("!/foo/\n/foo/\n").includes("foo/f.txt"));
		// ...and a later exclude removes an earlier inclusion.
		assert!(!non_cone("/foo/\n!/foo/\n").includes("foo/f.txt"));
	}

	#[test]
	fn non_cone_dir_only_negation_does_not_touch_a_deeper_file() {
		// Probed surprise: `!baz/` (dir-only, basename) does NOT exclude the file `baz/d.log` that an
		// earlier `*.log` included — a dir-only pattern never matches the file itself, and `baz` is the
		// file's own directory, not re-included by anything after, yet the file's include still stands.
		let m = non_cone("*.log\n!baz/\n");
		assert!(m.includes("baz/d.log"));
	}
}
