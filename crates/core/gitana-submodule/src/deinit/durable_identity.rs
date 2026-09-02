use gitana_fs_native::EntryIdentity;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DurableIdentity {
	device: u64,
	inode: u64,
}

impl From<EntryIdentity> for DurableIdentity {
	fn from(identity: EntryIdentity) -> Self {
		let (device, inode) = identity.parts();
		Self { device, inode }
	}
}

impl From<DurableIdentity> for EntryIdentity {
	fn from(identity: DurableIdentity) -> Self {
		Self::from_parts(identity.device, identity.inode)
	}
}
