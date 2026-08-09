//! A from-scratch git configuration model, parser, and serializer.
//!
//! Models the git-config file format faithfully: case-insensitive sections and
//! variable names, case-sensitive subsections, multi-valued variables, double
//! quoting and the `\n` `\t` `\b` `\"` `\\` escapes, trailing-backslash line
//! continuation, and `#`/`;` comments. Typed getters apply git's boolean and
//! integer rules.
//!
//! A [`GitConfigSource`] models one file; a [`GitConfig`] layers several of them (system, global,
//! local) into git's precedence stack, resolving reads across the layers while directing writes at a
//! single designated file.
//!
//! [`GitConfigSource::expand_includes`] performs git's `[include]` / `includeIf` expansion — an
//! inline splice of each included file at the directive's position — driven by a caller-supplied
//! [`IncludeResolver`] and [`IncludeContext`], so the crate stays I/O-free and wasm-pure.
//!
//! [`GitConfigBytes`] is the read-only counterpart for a single file that may contain arbitrary value
//! bytes. It retains direct values byte-for-byte without adding include expansion, layering, or writes.
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
mod config_bytes;
mod error;
mod include;
mod parser;
mod source;

pub use self::config::GitConfig;
pub use self::config_bytes::GitConfigBytes;
pub use self::error::ConfigError;
pub use self::include::{IncludeContext, IncludeResolver};
pub use self::source::{GitConfigSource, RemoteUrlScan};

pub(crate) use self::include::{condition_matches, is_hasconfig_remote_url, resolve_include_path};
