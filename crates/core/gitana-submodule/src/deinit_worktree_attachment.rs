use serde::{Deserialize, Serialize};

use crate::ConfigIdentity;

/// Durable namespace proof for the `core.worktree` attachment removed by deinit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeinitWorktreeAttachment {
	value: String,
	parent: ConfigIdentity,
	target: Option<ConfigIdentity>,
	symlinks: Vec<ConfigIdentity>,
}

impl DeinitWorktreeAttachment {
	pub fn new(
		value: String,
		parent: (u64, u64),
		target: Option<(u64, u64)>,
		symlinks: Vec<(u64, u64)>,
	) -> Self {
		Self {
			value,
			parent: ConfigIdentity::new(parent.0, parent.1),
			target: target.map(|(device, inode)| ConfigIdentity::new(device, inode)),
			symlinks: symlinks
				.into_iter()
				.map(|(device, inode)| ConfigIdentity::new(device, inode))
				.collect(),
		}
	}

	pub fn value(&self) -> &str {
		&self.value
	}

	pub fn parent(&self) -> (u64, u64) {
		self.parent.parts()
	}

	pub fn target(&self) -> Option<(u64, u64)> {
		self.target.as_ref().map(ConfigIdentity::parts)
	}

	pub fn symlinks(&self) -> Vec<(u64, u64)> {
		self.symlinks.iter().map(ConfigIdentity::parts).collect()
	}
}
