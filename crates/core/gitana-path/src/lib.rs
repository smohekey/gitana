//! Byte-preserving repository-relative Git paths.

#![forbid(unsafe_code)]

mod git_path;
mod git_path_component;
mod git_path_error;
mod git_pathspec;
mod git_tree_entry_name;
mod git_tree_path;

pub use self::{
	git_path::GitPath, git_path_component::GitPathComponent, git_path_error::GitPathError,
	git_pathspec::GitPathspec, git_tree_entry_name::GitTreeEntryName, git_tree_path::GitTreePath,
};

fn write_quoted(bytes: &[u8], formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
	formatter.write_str(&quote_bytes(bytes))
}

fn write_human(bytes: &[u8], formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
	match std::str::from_utf8(bytes) {
		Ok(text) => formatter.write_str(text),
		Err(_) => write_quoted(bytes, formatter),
	}
}

/// Render arbitrary Git-controlled bytes with reversible C-style quoting.
///
/// Printable ASCII is kept as-is. Control bytes, quotes, backslashes, and every non-ASCII byte are
/// escaped, so the returned UTF-8 string preserves exact byte identity at machine boundaries.
#[must_use]
pub fn quote_bytes(bytes: &[u8]) -> String {
	String::from_utf8(render_bytes(bytes, true)).expect("safe Git quoting is ASCII")
}

pub(crate) fn render_bytes(bytes: &[u8], quote_non_ascii: bool) -> Vec<u8> {
	let needs_quotes = bytes.iter().any(|byte| {
		matches!(*byte, 0..=0x1f | 0x7f | b'"' | b'\\') || (quote_non_ascii && !byte.is_ascii())
	});
	if !needs_quotes {
		return bytes.to_vec();
	}

	let mut out = Vec::with_capacity(bytes.len() + 2);
	out.push(b'"');
	for byte in bytes {
		match byte {
			b'\x07' => out.extend_from_slice(b"\\a"),
			b'\x08' => out.extend_from_slice(b"\\b"),
			b'\t' => out.extend_from_slice(b"\\t"),
			b'\n' => out.extend_from_slice(b"\\n"),
			b'\x0b' => out.extend_from_slice(b"\\v"),
			b'\x0c' => out.extend_from_slice(b"\\f"),
			b'\r' => out.extend_from_slice(b"\\r"),
			b'"' => out.extend_from_slice(b"\\\""),
			b'\\' => out.extend_from_slice(b"\\\\"),
			0..=0x1f | 0x7f => out.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
			128..=255 if quote_non_ascii => {
				out.extend_from_slice(format!("\\{byte:03o}").as_bytes());
			}
			byte => out.push(*byte),
		}
	}
	out.push(b'"');
	out
}

#[cfg(test)]
mod tests {
	use super::quote_bytes;

	#[test]
	fn arbitrary_bytes_are_quoted_reversibly() {
		let raw = quote_bytes(b"raw-\xff/**");
		let literal = quote_bytes(b"\"raw-\\377/**\"");

		assert_eq!(raw, "\"raw-\\377/**\"");
		assert_ne!(raw, literal);
	}
}
