use std::io::Write;
use std::path::Path;

use crate::{Backend, PathQuoteMode};
use anyhow::Result;
use gitana_diff::Edit;
use gitana_object::HashAlgorithm;
use gitana_worktree::{FileDiff, WorkTree};

use crate::dispatch::{self, WorkTreeCommand};

/// Number of unchanged context lines shown around each change (git's default).
const CONTEXT: usize = 3;

/// Show changes between the index and the working tree, or (with `cached`) between
/// `HEAD` and the index. Output is gta's own unified-diff form.
pub async fn run(cwd: &Path, cached: bool, quote_path: PathQuoteMode) -> Result<()> {
	dispatch::on_worktree(cwd, Diff { cached, quote_path }).await
}

struct Diff {
	cached: bool,
	quote_path: PathQuoteMode,
}

impl WorkTreeCommand for Diff {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		_prefix: gitana_path::GitPath,
	) -> Result<()> {
		let config = worktree.repository().effective_config().await?;
		let configured_quote_path = config
			.get_bool_validated("core", None, "quotepath")?
			.unwrap_or(true);
		let quote_path = match self.quote_path {
			PathQuoteMode::Config => configured_quote_path,
			PathQuoteMode::Always => true,
		};
		let files = if self.cached {
			worktree.diff_staged().await?
		} else {
			worktree.diff_unstaged().await?
		};

		let mut out = Vec::new();
		for file in &files {
			format_file(&mut out, file, quote_path);
		}
		std::io::stdout().write_all(&out)?;
		Ok(())
	}
}

pub(crate) fn format_file(out: &mut Vec<u8>, file: &FileDiff, quote_non_ascii: bool) {
	let path = &file.path;
	let old = file.old.as_ref();
	let new = file.new.as_ref();
	let old_path = path.render_with_affixes(b"a/", b"", quote_non_ascii);
	let new_path = path.render_with_affixes(b"b/", b"", quote_non_ascii);

	out.extend_from_slice(b"diff --git ");
	out.extend_from_slice(&old_path);
	out.push(b' ');
	out.extend_from_slice(&new_path);
	out.push(b'\n');
	if let (Some((_, om)), Some((_, nm))) = (old, new)
		&& om != nm
	{
		push(out, &format!("old mode {om:06o}\nnew mode {nm:06o}\n"));
	}

	let old_bytes = old.map(|(c, _)| c.as_slice()).unwrap_or(&[]);
	let new_bytes = new.map(|(c, _)| c.as_slice()).unwrap_or(&[]);
	if is_binary(old_bytes) || is_binary(new_bytes) {
		out.extend_from_slice(b"Binary files ");
		out.extend_from_slice(&old_path);
		out.extend_from_slice(b" and ");
		out.extend_from_slice(&new_path);
		out.extend_from_slice(b" differ\n");
		return;
	}

	let from: &[u8] = if old.is_some() {
		&old_path
	} else {
		b"/dev/null"
	};
	let to: &[u8] = if new.is_some() {
		&new_path
	} else {
		b"/dev/null"
	};
	out.extend_from_slice(b"--- ");
	out.extend_from_slice(from);
	out.extend_from_slice(b"\n+++ ");
	out.extend_from_slice(to);
	out.push(b'\n');

	let old_lines: Vec<&[u8]> = lines(old_bytes);
	let new_lines: Vec<&[u8]> = lines(new_bytes);
	let edits = gitana_diff::diff(&old_lines, &new_lines);
	emit_hunks(out, &edits, &old_lines, &new_lines);
}

/// Group changes into hunks (merging those within `2*CONTEXT` lines) and write them.
fn emit_hunks(out: &mut Vec<u8>, edits: &[Edit], old: &[&[u8]], new: &[&[u8]]) {
	let changes: Vec<usize> = edits
		.iter()
		.enumerate()
		.filter(|(_, e)| !matches!(e, Edit::Equal { .. }))
		.map(|(i, _)| i)
		.collect();
	if changes.is_empty() {
		return;
	}

	let mut i = 0;
	while i < changes.len() {
		let mut j = i;
		while j + 1 < changes.len() && changes[j + 1] - changes[j] <= 2 * CONTEXT + 1 {
			j += 1;
		}
		let start = changes[i].saturating_sub(CONTEXT);
		let end = (changes[j] + CONTEXT + 1).min(edits.len());
		emit_hunk(out, &edits[start..end], &edits[..start], old, new);
		i = j + 1;
	}
}

fn emit_hunk(out: &mut Vec<u8>, hunk: &[Edit], before: &[Edit], old: &[&[u8]], new: &[&[u8]]) {
	let old_len = hunk.iter().filter(|e| has_a(e)).count();
	let new_len = hunk.iter().filter(|e| has_b(e)).count();
	let old_start = hunk
		.iter()
		.find_map(a_index)
		.map(|a| a + 1)
		.unwrap_or_else(|| before.iter().filter(|e| has_a(e)).count());
	let new_start = hunk
		.iter()
		.find_map(b_index)
		.map(|b| b + 1)
		.unwrap_or_else(|| before.iter().filter(|e| has_b(e)).count());

	push(
		out,
		&format!(
			"@@ -{} +{} @@\n",
			range(old_start, old_len),
			range(new_start, new_len)
		),
	);
	for edit in hunk {
		match *edit {
			Edit::Equal { a, .. } => emit_line(out, b' ', old[a]),
			Edit::Delete { a } => emit_line(out, b'-', old[a]),
			Edit::Insert { b } => emit_line(out, b'+', new[b]),
		}
	}
}

fn emit_line(out: &mut Vec<u8>, prefix: u8, line: &[u8]) {
	let (text, has_nl) = match line.strip_suffix(b"\n") {
		Some(text) => (text, true),
		None => (line, false),
	};
	out.push(prefix);
	out.extend_from_slice(text);
	out.push(b'\n');
	if !has_nl {
		out.extend_from_slice(b"\\ No newline at end of file\n");
	}
}

/// Format a hunk range as git does: `start` when the length is 1, else `start,len`.
fn range(start: usize, len: usize) -> String {
	if len == 1 {
		start.to_string()
	} else {
		format!("{start},{len}")
	}
}

/// Split content into lines, each keeping its trailing newline (the last line may
/// not have one). Empty content yields no lines.
fn lines(bytes: &[u8]) -> Vec<&[u8]> {
	if bytes.is_empty() {
		Vec::new()
	} else {
		bytes.split_inclusive(|&b| b == b'\n').collect()
	}
}

fn is_binary(bytes: &[u8]) -> bool {
	// git samples the first 8000 bytes for a NUL.
	bytes.iter().take(8000).any(|&b| b == 0)
}

fn has_a(e: &Edit) -> bool {
	matches!(e, Edit::Equal { .. } | Edit::Delete { .. })
}

fn has_b(e: &Edit) -> bool {
	matches!(e, Edit::Equal { .. } | Edit::Insert { .. })
}

fn a_index(e: &Edit) -> Option<usize> {
	match *e {
		Edit::Equal { a, .. } | Edit::Delete { a } => Some(a),
		Edit::Insert { .. } => None,
	}
}

fn b_index(e: &Edit) -> Option<usize> {
	match *e {
		Edit::Equal { b, .. } | Edit::Insert { b } => Some(b),
		Edit::Delete { .. } => None,
	}
}

fn push(out: &mut Vec<u8>, text: &str) {
	out.extend_from_slice(text.as_bytes());
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn quotes_complete_prefixed_diff_paths() {
		let file = FileDiff {
			path: gitana_path::GitPath::from_utf8("line\nfile")
				.unwrap()
				.into(),
			old: Some((b"old\n".to_vec(), 0o100644)),
			new: Some((b"new\n".to_vec(), 0o100644)),
		};
		let mut output = Vec::new();
		format_file(&mut output, &file, true);
		let output = String::from_utf8(output).unwrap();
		assert!(output.starts_with(
			"diff --git \"a/line\\nfile\" \"b/line\\nfile\"\n--- \"a/line\\nfile\"\n+++ \"b/line\\nfile\"\n"
		));
	}
}
