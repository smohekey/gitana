use serde::{Deserialize, Serialize};

use crate::ConfigIdentity;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
enum ConfigTargetEntry {
	Missing,
	File { device: u64, inode: u64 },
}

/// Durable identity of one fully resolved repository-local config namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeinitConfigTarget {
	parent: ConfigIdentity,
	entry: ConfigTargetEntry,
	symlinks: Vec<ConfigIdentity>,
}

impl DeinitConfigTarget {
	pub fn new(parent: (u64, u64), target: Option<(u64, u64)>, symlinks: Vec<(u64, u64)>) -> Self {
		let entry = match target {
			Some((device, inode)) => ConfigTargetEntry::File { device, inode },
			None => ConfigTargetEntry::Missing,
		};
		Self {
			parent: ConfigIdentity::new(parent.0, parent.1),
			entry,
			symlinks: symlinks
				.into_iter()
				.map(|(device, inode)| ConfigIdentity::new(device, inode))
				.collect(),
		}
	}

	pub fn parent(&self) -> (u64, u64) {
		self.parent.parts()
	}

	pub fn target(&self) -> Option<(u64, u64)> {
		match self.entry {
			ConfigTargetEntry::Missing => None,
			ConfigTargetEntry::File { device, inode } => Some((device, inode)),
		}
	}

	pub fn symlinks(&self) -> Vec<(u64, u64)> {
		self.symlinks.iter().map(ConfigIdentity::parts).collect()
	}
}
