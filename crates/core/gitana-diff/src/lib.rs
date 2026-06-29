//! Pure text-diff and three-way-merge primitives, shared by the `gta diff` command and the
//! repository's tree-merge engine.
//!
//! - [`diff`] is the Myers O(ND) line diff git uses by default.
//! - [`merge`] is a diff3 three-way line merge built on it.
//!
//! Both operate on slices and own no I/O, so they have no dependencies.

mod merge;
mod myers;

pub use self::merge::{MergeOutcome, is_binary, merge};
pub use self::myers::{Edit, diff};
