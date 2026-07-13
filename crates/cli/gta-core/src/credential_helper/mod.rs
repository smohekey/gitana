//! git's credential-helper protocol: consulting and persisting credentials through external
//! `git-credential-*` programs (osxkeychain, store, manager, cache, …).
//!
//! [`CliCredentialProvider`](super::CliCredentialProvider) uses this to resolve which helpers a
//! request configures ([`resolve`]), then drives each [`Helper`] over git's `get`/`store`/`erase`
//! wire protocol. gitana speaks the protocol rather than reimplementing any keychain — the same
//! decision as slice 1's "don't reinvent credential storage".

mod helper;
mod resolve;

pub(crate) use self::helper::{GetOutput, Helper};
pub(crate) use self::resolve::{percent_encode_request_path, resolve};
