//! Capability-scoped native filesystem namespace transactions.
//!
//! The standard library has no portable no-replace rename and no identity-conditioned replace.
//! This crate keeps those platform details behind retained [`cap_std::fs::Dir`] capabilities.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

mod entry_identity;
mod namespace;
mod path;
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

pub use self::entry_identity::{EntryIdentity, directory_identity, entry_identity, file_identity};
pub use self::namespace::{
	remove_dir_all_if_identity, remove_dir_if_identity, remove_file_if_identity, rename_noreplace,
	rename_noreplace_if_identity, replace_if_identities, replace_if_identity,
};
pub use self::path::{paths_equivalent, strip_path_prefix};
