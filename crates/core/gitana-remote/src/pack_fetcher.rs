//! The fetch-negotiation seam shared by Smart HTTP and SSH remotes.

use std::future::Future;

use anyhow::Result;
use gitana_file_store::FileStore;
use gitana_git_http::Deepen;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

/// Downloads the objects a fetch needs into a repository, over some transport.
///
/// Fetch negotiation differs by transport: Smart HTTP is stateless-RPC (each round re-POSTs the full
/// have-set — [`HttpPackFetcher`](crate::HttpPackFetcher)), while SSH is a single stateful stream that
/// reads the server's ACK batch after each have-group ([`SshPackFetcher`](crate::SshPackFetcher)). This
/// seam lets `gitana-porcelain`'s `fetch`/`pull` drive either without knowing which — the whole
/// negotiation loop lives inside the implementation.
pub trait PackFetcher {
	/// Download the objects reachable from `wants` but not from `haves` (the local ref tips) into `repo`.
	/// `deepen` requests a shallow history; `include_tag` asks for reachable annotated tags.
	fn fetch_pack<F: FileStore, H: HashAlgorithm>(
		&mut self,
		repo: &Repository<F, H>,
		wants: &[ObjectId<H>],
		haves: &[ObjectId<H>],
		deepen: &Deepen,
		include_tag: bool,
	) -> impl Future<Output = Result<()>>;
}
