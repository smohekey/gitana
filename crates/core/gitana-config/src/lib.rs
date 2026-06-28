//! A from-scratch git configuration model, parser, and serializer.
//!
//! Models the git-config file format faithfully: case-insensitive sections and
//! variable names, case-sensitive subsections, multi-valued variables, double
//! quoting and the `\n` `\t` `\b` `\"` `\\` escapes, trailing-backslash line
//! continuation, and `#`/`;` comments. Typed getters apply git's boolean and
//! integer rules. Includes/`includeIf` are out of scope.

mod config;
mod error;
mod parser;

pub use config::GitConfig;
pub use error::ConfigError;
