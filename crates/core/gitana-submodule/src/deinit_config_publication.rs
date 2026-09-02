use serde::{Deserialize, Serialize};

/// Durable identity of a private config image prepared for one deinit transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeinitConfigPublication {
	pub name: String,
	pub device: u64,
	pub inode: u64,
}

impl DeinitConfigPublication {
	pub fn new(name: String, device: u64, inode: u64) -> Self {
		Self {
			name,
			device,
			inode,
		}
	}
}
