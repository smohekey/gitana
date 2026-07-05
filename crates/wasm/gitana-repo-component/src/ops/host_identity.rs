//! Identity backed by host-supplied lines, for `commit`.

use anyhow::Result;
use gitana_porcelain::Identity;

/// A [`gitana_porcelain::Identity`] backed by the raw author/committer lines the host passes to
/// `commit`. The component reads no process env or clock, so identity is fixed up front and never
/// fails — the lazy `Result`/`_or_default` resolution the trait allows collapses to trivial returns.
pub(crate) struct HostIdentity<'a> {
	pub author: &'a str,
	pub committer: &'a str,
}

impl Identity for HostIdentity<'_> {
	async fn author(&self) -> Result<String> {
		Ok(self.author.to_owned())
	}

	async fn committer(&self) -> Result<String> {
		Ok(self.committer.to_owned())
	}

	async fn committer_or_default(&self) -> String {
		self.committer.to_owned()
	}
}
