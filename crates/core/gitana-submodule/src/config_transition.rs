use serde::{Deserialize, Serialize};

use crate::{DeinitConfigTarget, DeinitWorktreeAttachment};

/// Secret-free fingerprints of one repository-local configuration transition.
///
/// The native configuration provider plans the exact before/after byte states before a durable
/// filesystem operation begins. Recovery accepts only one of those states, so it cannot silently
/// fold an unrelated config edit into the pending operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeinitConfigTransition {
	pub before_fingerprint: String,
	pub after_fingerprint: String,
	pub target: DeinitConfigTarget,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub worktree_attachment: Option<DeinitWorktreeAttachment>,
}

impl DeinitConfigTransition {
	pub fn changes(&self) -> bool {
		self.before_fingerprint != self.after_fingerprint
	}
}
