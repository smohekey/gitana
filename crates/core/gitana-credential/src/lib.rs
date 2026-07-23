//! git's HTTP credential resolution, extracted so the CLI and Code Henge share one implementation.
//!
//! [`HelperChainProvider`] implements [`gitana_remote::CredentialProvider`] over git's own
//! credential-helper chain: it resolves which helpers a request configures ([`resolve`]) and drives
//! each over git's `get`/`store`/`erase` wire protocol ([`helper`]). It is **headless** — it never
//! prompts, so a gap the helpers leave is returned as "no credential" and the caller stays anonymous.
//!
//! gitana speaks the helper protocol rather than reimplementing any keychain — the same decision as
//! not reinventing credential storage. The `gta` CLI wraps this with an interactive prompt fallback
//! (via [`run_chain`](HelperChainProvider::run_chain) + [`ChainOutcome`]); Code Henge wraps it
//! read-only (headless `fill`, no-op `approve`/`reject`).

mod helper;
mod provider;
mod resolve;

pub use self::provider::{ChainOutcome, HelperChainProvider};
pub use self::resolve::percent_encode_request_path;
