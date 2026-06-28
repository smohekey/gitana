use crate::ObjectError;

/// The four git object kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
	Blob,
	Tree,
	Commit,
	Tag,
}

impl ObjectKind {
	/// The kind's git wire name (`blob`, `tree`, `commit`, `tag`).
	pub fn as_str(self) -> &'static str {
		match self {
			ObjectKind::Blob => "blob",
			ObjectKind::Tree => "tree",
			ObjectKind::Commit => "commit",
			ObjectKind::Tag => "tag",
		}
	}

	/// Parse a kind from its git wire name.
	pub fn from_wire(name: &[u8]) -> Result<Self, ObjectError> {
		match name {
			b"blob" => Ok(ObjectKind::Blob),
			b"tree" => Ok(ObjectKind::Tree),
			b"commit" => Ok(ObjectKind::Commit),
			b"tag" => Ok(ObjectKind::Tag),
			_ => Err(ObjectError::MalformedHeader),
		}
	}
}
