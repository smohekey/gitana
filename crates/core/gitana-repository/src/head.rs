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
			.map_err(|_| RepositoryError::InvalidRef("HEAD is not UTF-8".to_owned()))?;
		// Strip only trailing line terminators — the content must *start* with `ref:` to be symbolic (git
		// tolerates no leading whitespace; e.g. a leading NBSP makes it a non-symbolic, then-invalid HEAD).
		let text = text.trim_end_matches(['\n', '\r']);
		// git accepts a symbolic ref whose `ref:` is followed by a space, a tab, or nothing
		// (`ref: refs/heads/main`, `ref:refs/heads/main`, `ref:\trefs/...`) — but **only** space/tab, not
		// vertical-tab / form-feed / Unicode whitespace (which git rejects the repository over). So strip only
		// space/tab, leaving any other separator byte as part of the (then-invalid) refname. An **empty**
		// target (`ref:` alone) is rejected — git treats it as a broken repository.
		if let Some(target) = text.strip_prefix("ref:") {
			let target = target.trim_matches([' ', '\t']);
			if target.is_empty() {
				return Err(RepositoryError::InvalidRef(
					"HEAD: empty symbolic ref".to_owned(),
				));
			}
			Ok(HeadState::Symbolic(target.to_owned()))
		} else {
			let id = ObjectId::from_hex(text.trim_matches([' ', '\t']))
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

	#[test]
	fn parses_symbolic_head_with_any_whitespace_after_ref() {
		// git strips `ref:` then skips whitespace, so the no-space and tab forms are valid symrefs too.
		let want = HeadState::<Sha256>::Symbolic("refs/heads/main".to_owned());
		for form in [
			"ref: refs/heads/main",
			"ref:refs/heads/main",
			"ref:\trefs/heads/main\n",
		] {
			assert_eq!(HeadState::parse(form.as_bytes()).unwrap(), want);
		}
	}

	#[test]
	fn rejects_an_empty_symbolic_head_target() {
		// git treats `ref:` with no target as a broken repository; parsing must error, not yield `Symbolic("")`.
		for form in ["ref:", "ref: ", "ref:\t\n"] {
			assert!(HeadState::<Sha256>::parse(form.as_bytes()).is_err());
		}
	}

	#[test]
	fn keeps_non_space_tab_separators_in_the_target() {
		// git accepts only space/tab after `ref:` — a vertical tab or NBSP separator makes the repository
		// invalid. The parser must not strip those (as Rust's `trim` would); leaving them in the target lets
		// downstream refname validation reject it rather than silently normalizing to a healthy symref.
		for form in ["ref:\x0brefs/heads/main", "ref:\u{a0}refs/heads/main"] {
			match HeadState::<Sha256>::parse(form.as_bytes()).unwrap() {
				HeadState::Symbolic(target) => assert!(
					!target.starts_with("refs/"),
					"the non-space/tab separator must remain in the target: {target:?}"
				),
				other => panic!("expected symbolic, got {other:?}"),
			}
		}
	}
}
