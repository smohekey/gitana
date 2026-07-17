//! A from-scratch git configuration model, parser, and serializer.
//!
//! Models the git-config file format faithfully: case-insensitive sections and
//! variable names, case-sensitive subsections, multi-valued variables, double
//! quoting and the `\n` `\t` `\b` `\"` `\\` escapes, trailing-backslash line
//! continuation, and `#`/`;` comments. Typed getters apply git's boolean and
//! integer rules. Includes/`includeIf` are out of scope.
//!
//! A [`GitConfigSource`] models one file; a [`GitConfig`] layers several of them (system, global,
//! local) into git's precedence stack, resolving reads across the layers while directing writes at a
//! single designated file.
//!
//! **These types' `Debug` renders every value verbatim** (and again in each element's `raw` text), so a
//! `{:?}` on a config carrying an `http.extraHeader` bearer token or a tokenized remote URL discloses the
//! secret into whatever log or error chain it reaches.
//!
//! **Borrowing does not help:** `&GitConfig` is `Debug` exactly because `GitConfig` is, so `{:?}` on a
//! reference — or a `Debug`-deriving struct holding a `&'a GitConfig` field — leaks identically. The risk
//! is *being debug-formatted*, owned or borrowed; the only thing that makes a reference safer is never
//! formatting it. A type that holds a config either way should hand-write a redacting `Debug`, as
//! `gitana-linked-worktree`'s `WorktreeContext` does. See `docs/hlds/gitana-config-followups.md`, which
//! also records the known divergences from git noted above (`include`/`includeIf`, and a leading BOM).

mod config;
mod error;
mod parser;
mod source;

pub use config::GitConfig;
pub use error::ConfigError;
pub use source::GitConfigSource;
