use crate::GitHttpError;

/// A git wire service, named by its `service=` query parameter / URL suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
	/// Fetch side (clone / fetch / pull, and ref discovery).
	UploadPack,
	/// Push side (receive-pack).
	ReceivePack,
}

impl Service {
	/// The wire name (`git-upload-pack` / `git-receive-pack`).
	pub fn as_str(self) -> &'static str {
		match self {
			Service::UploadPack => "git-upload-pack",
			Service::ReceivePack => "git-receive-pack",
		}
	}

	/// The `Content-Type` for this service's `GET /info/refs` advertisement.
	pub fn advertisement_content_type(self) -> String {
		format!("application/x-{}-advertisement", self.as_str())
	}

	/// The `Content-Type` for this service's `POST` result body.
	pub fn result_content_type(self) -> String {
		format!("application/x-{}-result", self.as_str())
	}

	/// Parse a `service=` value (e.g. from `?service=git-upload-pack`).
	pub fn parse(value: &str) -> Result<Self, GitHttpError> {
		match value {
			"git-upload-pack" => Ok(Service::UploadPack),
			"git-receive-pack" => Ok(Service::ReceivePack),
			other => Err(GitHttpError::UnsupportedService(other.to_owned())),
		}
	}
}

/// The git wire protocol version negotiated for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolVersion {
	/// The original stateless-RPC protocol (refs advertised in `GET /info/refs`).
	V0,
	/// Protocol v2 (capability advertisement in `GET /info/refs`; refs via `ls-refs`).
	V2,
}

impl ProtocolVersion {
	/// Negotiate from a `Git-Protocol` header value (e.g. `version=2`), defaulting to
	/// v2 — the modern default — when unset or unrecognized.
	///
	/// A client that wants v0 sends no `version=2`, which any value other than a
	/// `version=2` token selects.
	pub fn from_header(value: Option<&str>) -> Self {
		match value {
			Some(header) if header.split(':').any(|kv| kv.trim() == "version=2") => ProtocolVersion::V2,
			Some(_) => ProtocolVersion::V0,
			None => ProtocolVersion::V0,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_services() {
		assert_eq!(
			Service::parse("git-upload-pack").unwrap(),
			Service::UploadPack
		);
		assert_eq!(
			Service::parse("git-receive-pack").unwrap(),
			Service::ReceivePack
		);
		assert!(matches!(
			Service::parse("git-evil"),
			Err(GitHttpError::UnsupportedService(_))
		));
	}

	#[test]
	fn negotiates_protocol_version() {
		assert_eq!(
			ProtocolVersion::from_header(Some("version=2")),
			ProtocolVersion::V2
		);
		// git sends key:value pairs separated by ':'.
		assert_eq!(
			ProtocolVersion::from_header(Some("version=2:agent=git/2.0")),
			ProtocolVersion::V2
		);
		assert_eq!(
			ProtocolVersion::from_header(Some("version=1")),
			ProtocolVersion::V0
		);
		assert_eq!(ProtocolVersion::from_header(None), ProtocolVersion::V0);
	}
}
