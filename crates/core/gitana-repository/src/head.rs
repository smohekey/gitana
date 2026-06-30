use gitana_object::{HashAlgorithm, ObjectId};

use crate::RepositoryError;

/// The state of `HEAD`: a symbolic ref (normal) or a detached commit id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState<H: HashAlgorithm> {
	/// `HEAD` points at a ref name, e.g. `refs/heads/main`. The ref may not exist
	/// yet (an unborn branch on a fresh repo).
	Symbolic(String),
	/// `HEAD` holds a raw commit id (detached HEAD).
	Detached(ObjectId<H>),
}

impl<H: HashAlgorithm> HeadState<H> {
	/// Parse the bytes of a `HEAD` file.
	pub fn parse(bytes: &[u8]) -> Result<Self, RepositoryError> {
		let text = std::str::from_utf8(bytes)
			.map_err(|_| RepositoryError::InvalidRef("HEAD is not UTF-8".to_owned()))?
			.trim();
		if let Some(target) = text.strip_prefix("ref: ") {
			Ok(HeadState::Symbolic(target.trim().to_owned()))
		} else {
			let id = ObjectId::from_hex(text)
				.map_err(|_| RepositoryError::InvalidRef(format!("HEAD: {text}")))?;
			Ok(HeadState::Detached(id))
		}
	}

	/// Render to the bytes of a `HEAD` file (with trailing newline).
	pub fn render(&self) -> String {
		match self {
			HeadState::Symbolic(target) => format!("ref: {target}\n"),
			HeadState::Detached(id) => format!("{id}\n"),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use gitana_object::Sha256;

	#[test]
	fn round_trips_symbolic_and_detached() {
		let sym = HeadState::<Sha256>::Symbolic("refs/heads/main".to_owned());
		assert_eq!(HeadState::parse(sym.render().as_bytes()).unwrap(), sym);

		let id = ObjectId::<Sha256>::compute(gitana_object::ObjectKind::Commit, b"c");
		let det = HeadState::Detached(id);
		assert_eq!(HeadState::parse(det.render().as_bytes()).unwrap(), det);
	}
}
