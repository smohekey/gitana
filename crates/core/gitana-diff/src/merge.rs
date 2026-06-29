//! Diff3 three-way line merge built on the Myers line diff.

use std::collections::HashMap;

use crate::myers::{Edit, diff};

/// The result of a three-way merge: the merged bytes, and whether any region conflicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
	pub content: Vec<u8>,
	pub conflicted: bool,
}

/// Three-way merge `ours` and `theirs` against their common `base`, line by line.
///
/// A region changed on only one side is taken from that side; a region changed identically on both
/// is taken once; a region changed differently on both becomes a conflict hunk delimited by
/// `<<<<<<< {ours_label}` / `=======` / `>>>>>>> {theirs_label}`. Lines keep their trailing
/// newline, so a missing final newline round-trips.
pub fn merge(
	base: &[u8],
	ours: &[u8],
	theirs: &[u8],
	ours_label: &str,
	theirs_label: &str,
) -> MergeOutcome {
	let base_lines = split_lines(base);
	let ours_lines = split_lines(ours);
	let theirs_lines = split_lines(theirs);

	// Base line -> matching line on each side (only where the line is unchanged on that side).
	let to_theirs: HashMap<usize, usize> = equal_pairs(&diff(&base_lines, &theirs_lines))
		.into_iter()
		.collect();
	// Sync points: base lines unchanged on *both* sides, hence common to all three.
	let mut syncs: Vec<(usize, usize, usize)> = equal_pairs(&diff(&base_lines, &ours_lines))
		.into_iter()
		.filter_map(|(base_i, ours_i)| to_theirs.get(&base_i).map(|&t| (base_i, ours_i, t)))
		.collect();
	syncs.sort_unstable();
	// End sentinel so the final region (after the last sync) is emitted.
	syncs.push((base_lines.len(), ours_lines.len(), theirs_lines.len()));

	let mut out = Vec::new();
	let mut conflicted = false;
	let (mut pb, mut po, mut pt) = (0usize, 0usize, 0usize);
	for (sb, so, st) in syncs {
		let base_region = &base_lines[pb..sb];
		let ours_region = &ours_lines[po..so];
		let theirs_region = &theirs_lines[pt..st];

		if ours_region == base_region {
			push_lines(&mut out, theirs_region); // only theirs (if anything) changed here
		} else if theirs_region == base_region {
			push_lines(&mut out, ours_region); // only ours changed
		} else if ours_region == theirs_region {
			push_lines(&mut out, ours_region); // both made the same change
		} else {
			conflicted = true;
			push_marker(&mut out, "<<<<<<<", ours_label);
			push_lines(&mut out, ours_region);
			push_marker(&mut out, "=======", "");
			push_lines(&mut out, theirs_region);
			push_marker(&mut out, ">>>>>>>", theirs_label);
		}

		// Emit the stable line shared by all three (skip the end sentinel).
		if sb < base_lines.len() {
			out.extend_from_slice(base_lines[sb]);
		}
		pb = sb + 1;
		po = so + 1;
		pt = st + 1;
	}

	MergeOutcome {
		content: out,
		conflicted,
	}
}

/// git's binary heuristic: content is binary if it has a NUL byte within the first 8000 bytes.
/// Binary content cannot be line-merged, so callers should treat a divergence as a conflict rather
/// than splicing markers into it.
pub fn is_binary(data: &[u8]) -> bool {
	const FIRST_FEW_BYTES: usize = 8000;
	data.iter().take(FIRST_FEW_BYTES).any(|&byte| byte == 0)
}

/// The `(base_index, other_index)` pairs of lines kept unchanged by a diff.
fn equal_pairs(edits: &[Edit]) -> Vec<(usize, usize)> {
	edits
		.iter()
		.filter_map(|edit| match edit {
			Edit::Equal { a, b } => Some((*a, *b)),
			_ => None,
		})
		.collect()
}

/// Split into lines that keep their trailing `\n`; a final line without one is kept as-is.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
	let mut lines = Vec::new();
	let mut start = 0;
	for (i, &byte) in data.iter().enumerate() {
		if byte == b'\n' {
			lines.push(&data[start..=i]);
			start = i + 1;
		}
	}
	if start < data.len() {
		lines.push(&data[start..]);
	}
	lines
}

fn push_lines(out: &mut Vec<u8>, lines: &[&[u8]]) {
	for line in lines {
		out.extend_from_slice(line);
	}
}

/// Push a conflict marker on its own line (synthesising a newline first if the preceding region
/// lacked one, e.g. at end of file), with an optional label.
fn push_marker(out: &mut Vec<u8>, marker: &str, label: &str) {
	if out.last().is_some_and(|&b| b != b'\n') {
		out.push(b'\n');
	}
	out.extend_from_slice(marker.as_bytes());
	if !label.is_empty() {
		out.push(b' ');
		out.extend_from_slice(label.as_bytes());
	}
	out.push(b'\n');
}

#[cfg(test)]
mod tests {
	use super::*;

	fn merged(base: &str, ours: &str, theirs: &str) -> (String, bool) {
		let out = merge(
			base.as_bytes(),
			ours.as_bytes(),
			theirs.as_bytes(),
			"ours",
			"theirs",
		);
		(String::from_utf8(out.content).unwrap(), out.conflicted)
	}

	#[test]
	fn non_overlapping_edits_merge_cleanly() {
		let (text, conflicted) = merged("1\n2\n3\n4\n5\n", "A\n2\n3\n4\n5\n", "1\n2\n3\n4\nB\n");
		assert_eq!(text, "A\n2\n3\n4\nB\n");
		assert!(!conflicted);
	}

	#[test]
	fn identical_change_on_both_sides_is_clean() {
		let (text, conflicted) = merged("1\n2\n3\n", "1\nX\n3\n", "1\nX\n3\n");
		assert_eq!(text, "1\nX\n3\n");
		assert!(!conflicted);
	}

	#[test]
	fn divergent_change_conflicts() {
		let (text, conflicted) = merged("1\n2\n3\n", "1\nX\n3\n", "1\nY\n3\n");
		assert!(conflicted);
		assert_eq!(text, "1\n<<<<<<< ours\nX\n=======\nY\n>>>>>>> theirs\n3\n");
	}

	#[test]
	fn insertion_on_one_side_only() {
		let (text, conflicted) = merged("1\n2\n", "1\n1.5\n2\n", "1\n2\n");
		assert_eq!(text, "1\n1.5\n2\n");
		assert!(!conflicted);
	}

	#[test]
	fn deletion_on_one_side_only() {
		let (text, conflicted) = merged("1\n2\n3\n", "1\n3\n", "1\n2\n3\n");
		assert_eq!(text, "1\n3\n");
		assert!(!conflicted);
	}

	#[test]
	fn add_add_of_different_content_conflicts() {
		let (text, conflicted) = merged("", "ours line\n", "theirs line\n");
		assert!(conflicted);
		assert_eq!(
			text,
			"<<<<<<< ours\nours line\n=======\ntheirs line\n>>>>>>> theirs\n"
		);
	}

	#[test]
	fn missing_final_newline_is_preserved() {
		// theirs leaves the file unchanged; ours drops the trailing newline.
		let (text, conflicted) = merged("x\n", "x", "x\n");
		assert_eq!(text, "x");
		assert!(!conflicted);
	}

	#[test]
	fn is_binary_detects_nul_within_the_window() {
		assert!(is_binary(b"abc\0def"));
		assert!(!is_binary(b"plain text\nmore\n"));
		assert!(!is_binary(b""));
		// A NUL only past the first 8000 bytes is treated as text, like git.
		let mut late_nul = vec![b'a'; 8000];
		late_nul.push(0);
		assert!(!is_binary(&late_nul));
	}
}
