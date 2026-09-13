//! Capability-scoped native filesystem namespace transactions.
//!
//! The standard library has no portable no-replace rename and no identity-conditioned replace.
//! This crate keeps those platform details behind retained [`cap_std::fs::Dir`] capabilities.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

mod entry_identity;
mod namespace;
mod path;
#[allow(unsafe_code)]
mod process;
mod process_current_dir_guard;
mod process_file_guard;
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

pub use self::entry_identity::{EntryIdentity, directory_identity, entry_identity, file_identity};
pub use self::namespace::{
	remove_dir_all_if_identity, remove_dir_if_identity, remove_file_if_identity, rename_noreplace,
	rename_noreplace_if_identity, replace_if_identities, replace_if_identity,
};
pub use self::path::{lexical_normalize, paths_equivalent, strip_path_prefix};
pub use self::process::{configure_process_current_dir, configure_process_file, open_process_file};
pub use self::process_current_dir_guard::ProcessCurrentDirGuard;
pub use self::process_file_guard::ProcessFileGuard;
