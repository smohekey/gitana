use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConfigIdentity {
	device: u64,
	inode: u64,
}

impl ConfigIdentity {
	pub(crate) const fn new(device: u64, inode: u64) -> Self {
		Self { device, inode }
	}

	pub(crate) const fn parts(&self) -> (u64, u64) {
		(self.device, self.inode)
	}
}
