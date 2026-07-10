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

mod config;
mod error;
mod parser;
mod source;

pub use config::GitConfig;
pub use error::ConfigError;
pub use source::GitConfigSource;
