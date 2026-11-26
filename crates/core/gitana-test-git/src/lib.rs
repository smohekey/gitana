//! Stock `git` for gitana's oracle tests, run so that nothing from the developer's machine reaches it.
//!
//! The oracle tests build repositories with git and check gitana against it, so their results must not
//! depend on the machine they run on. Every git a test runs goes through [`git_command`], and each crate
//! that has moved onto it checks that with [`assert_git_spawned_only_through_git_command`] (see
//! `docs/conventions.md`, "Tests that run git").

mod git_command;
mod spawn_guard;

pub use self::git_command::git_command;
pub use self::spawn_guard::assert_git_spawned_only_through_git_command;
