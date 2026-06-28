use std::io::Write;
use std::path::Path;

use anyhow::Result;
use gitana_worktree::FileDiff;

use crate::repo;

mod myers;

use myers::Edit;

/// Number of unchanged context lines shown around each change (git's default).
const CONTEXT: usize = 3;

/// Show changes between the index and the working tree, or (with `cached`) between
/// `HEAD` and the index. Output is gta's own unified-diff form.
pub async fn run(cwd: &Path, cached: bool) -> Result<()> {
	let wt = repo::open_worktree(cwd)?;
	let files = if cached {
		wt.diff_staged().await?
	} else {
		wt.diff_unstaged().await?
	};

	let mut out = Vec::new();
	for file in &files {
		format_file(&mut out, file);
	}
	std::io::stdout().write_all(&out)?;
	Ok(())
}

fn format_file(out: &mut Vec<u8>, file: &FileDiff) {
	let path = &file.path;
	let old = file.old.as_ref();
	let new = file.new.as_ref();

	push(out, &format!("diff --git a/{path} b/{path}\n"));
	if let (Some((_, om)), Some((_, nm))) = (old, new)
		&& om != nm
	{
		push(out, &format!("old mode {om:06o}\nnew mode {nm:06o}\n"));
	}

	let old_bytes = old.map(|(c, _)| c.as_slice()).unwrap_or(&[]);
	let new_bytes = new.map(|(c, _)| c.as_slice()).unwrap_or(&[]);
	if is_binary(old_bytes) || is_binary(new_bytes) {
		push(out, &format!("Binary files a/{path} and b/{path} differ\n"));
		return;
	}

	let from = if old.is_some() {
		format!("a/{path}")
	} else {
		"/dev/null".to_owned()
	};
	let to = if new.is_some() {
		format!("b/{path}")
	} else {
		"/dev/null".to_owned()
	};
	push(out, &format!("--- {from}\n+++ {to}\n"));

	let old_lines: Vec<&[u8]> = lines(old_bytes);
	let new_lines: Vec<&[u8]> = lines(new_bytes);
	let edits = myers::diff(&old_lines, &new_lines);
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
