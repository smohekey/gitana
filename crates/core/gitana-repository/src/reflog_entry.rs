use gitana_object::{HashAlgorithm, ObjectId};

/// One line of a reflog, as git writes them: `<old> <new> <committer>\t<message>`. The committer
/// and message are kept as the bytes git wrote (a message may be any bytes a `-m` held).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflogEntry<H: HashAlgorithm> {
	/// The ref's value before the change; `None` for git's all-zero id (the ref didn't exist).
	pub old: Option<ObjectId<H>>,
	/// The ref's value after the change; `None` for git's all-zero id (the ref went).
	pub new: Option<ObjectId<H>>,
	/// The identity line (`Name <email> seconds ±hhmm`).
	pub committer: Vec<u8>,
	/// The message after the tab, if any.
	pub message: Vec<u8>,
}

impl<H: HashAlgorithm> ReflogEntry<H> {
	/// `line` (without its newline) read as git reads a reflog line; `None` for one git couldn't
	/// have written.
	pub(crate) fn parse(line: &[u8]) -> Option<Self> {
		let hex = H::RAW_LEN * 2;
		if line.len() < 2 * hex + 1
			|| line.get(hex) != Some(&b' ')
			|| line.get(2 * hex + 1) != Some(&b' ')
		{
			return None;
		}
		let id = |field: &[u8]| -> Option<Option<ObjectId<H>>> {
			let text = std::str::from_utf8(field).ok()?;
			if text.bytes().all(|byte| byte == b'0') {
				return Some(None);
			}
			ObjectId::from_hex(text).ok().map(Some)
		};
		let old = id(&line[..hex])?;
		let new = id(&line[hex + 1..2 * hex + 1])?;
		let rest = &line[2 * hex + 2..];
		let (committer, message) = match rest.iter().position(|byte| *byte == b'\t') {
			Some(tab) => (rest[..tab].to_vec(), rest[tab + 1..].to_vec()),
			None => (rest.to_vec(), Vec::new()),
		};
		Some(ReflogEntry {
			old,
			new,
			committer,
			message,
		})
	}
}
