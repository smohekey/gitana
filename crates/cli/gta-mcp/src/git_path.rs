use std::str::FromStr;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use gitana_path::GitPathspec;

const BYTES_TAG: &str = "gitana-path-v1:base64url:";
const REVISION_BYTES_TAG: &str = "gitana-revision-v1:base64url:";

/// A Git pathspec transported through MCP's string-only clap boundary.
///
/// Ordinary strings retain their UTF-8 bytes. Arbitrary Git bytes use the
/// canonical `gitana-path-v1:base64url:<unpadded-base64url>` spelling. A UTF-8
/// path that starts with the reserved tag can use that same encoded spelling.
#[derive(Clone)]
pub(crate) struct McpGitPath(GitPathspec);

impl McpGitPath {
	pub(crate) fn into_pathspec(self) -> GitPathspec {
		self.0
	}
}

/// A revision specification transported through MCP's string-only boundary.
///
/// Ordinary UTF-8 specs are unchanged. A spec containing arbitrary path bytes uses the canonical
/// `gitana-revision-v1:base64url:<unpadded-base64url>` spelling for the complete argument.
#[derive(Clone)]
pub(crate) struct McpRevisionSpec(Vec<u8>);

impl McpRevisionSpec {
	pub(crate) fn into_bytes(self) -> Vec<u8> {
		self.0
	}
}

impl FromStr for McpGitPath {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let bytes = if let Some(encoded) = value.strip_prefix(BYTES_TAG) {
			URL_SAFE_NO_PAD
				.decode(encoded)
				.map_err(|error| format!("invalid Git path byte encoding: {error}"))?
		} else {
			value.as_bytes().to_vec()
		};
		GitPathspec::from_bytes(bytes)
			.map(Self)
			.map_err(|error| error.to_string())
	}
}

impl FromStr for McpRevisionSpec {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let bytes = if let Some(encoded) = value.strip_prefix(REVISION_BYTES_TAG) {
			URL_SAFE_NO_PAD
				.decode(encoded)
				.map_err(|error| format!("invalid revision byte encoding: {error}"))?
		} else {
			value.as_bytes().to_vec()
		};
		if bytes.contains(&0) {
			return Err("revision specifications cannot contain NUL bytes".to_owned());
		}
		Ok(Self(bytes))
	}
}

pub(crate) fn into_pathspecs(paths: Vec<McpGitPath>) -> Vec<GitPathspec> {
	paths.into_iter().map(McpGitPath::into_pathspec).collect()
}

#[cfg(test)]
mod tests {
	use super::{McpGitPath, McpRevisionSpec};
	use std::str::FromStr;

	#[test]
	fn accepts_plain_utf8() {
		let path = McpGitPath::from_str("src/lib.rs").unwrap();
		assert_eq!(path.into_pathspec().as_bytes(), b"src/lib.rs");
	}

	#[test]
	fn decodes_exact_path_bytes_on_every_platform() {
		let path = McpGitPath::from_str("gitana-path-v1:base64url:cmF3Lf8").unwrap();
		assert_eq!(path.into_pathspec().as_bytes(), b"raw-\xff");
	}

	#[test]
	fn decodes_exact_revision_bytes_on_every_platform() {
		let spec = McpRevisionSpec::from_str("gitana-revision-v1:base64url:SEVBRDpyYXct_w").unwrap();
		assert_eq!(spec.into_bytes(), b"HEAD:raw-\xff");
	}
}
