//! An in-process pack fetcher over an injected local repository.

use anyhow::{Result, bail};
use gitana_file_store::FileStore;
use gitana_git_http::{
	Deepen, build_upload_pack_request, parse_upload_pack_response, upload_pack_v0,
};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_repository::Repository;

use crate::{PackFetcher, store_response};

/// A fetcher that serves objects from `source` without network or subprocess access.
pub struct LocalPackFetcher<F: FileStore, H: HashAlgorithm> {
	source: Repository<F, H>,
}

impl<F: FileStore, H: HashAlgorithm> LocalPackFetcher<F, H> {
	/// Wrap an already-opened source repository.
	pub fn new(source: Repository<F, H>) -> Self {
		Self { source }
	}
}

impl<SF: FileStore, SH: HashAlgorithm> PackFetcher for LocalPackFetcher<SF, SH> {
	async fn fetch_pack<F: FileStore, H: HashAlgorithm>(
		&mut self,
		repository: &Repository<F, H>,
		wants: &[ObjectId<H>],
		haves: &[ObjectId<H>],
		deepen: &Deepen,
		include_tag: bool,
	) -> Result<()> {
		if SH::NAME != H::NAME {
			bail!(
				"local fetch source uses {}, but the destination uses {}",
				SH::NAME,
				H::NAME
			);
		}
		if wants.is_empty() {
			return Ok(());
		}

		let shallow = repository.read_shallow().await?;
		let request = build_upload_pack_request(wants, haves, &shallow, deepen, include_tag, true);
		let response = upload_pack_v0(&self.source, &request).await?;
		let response = parse_upload_pack_response::<H>(&response)?;
		store_response(repository, &shallow, response).await?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::{ObjectId, Sha1, Sha256};
	use gitana_object_store::ObjectStore;

	use super::*;

	#[tokio::test]
	async fn refuses_a_mismatched_hash_algorithm() {
		let source = Repository::<_, Sha1>::new(ObjectStore::new(MemoryFileStore::new()));
		let destination = Repository::<_, Sha256>::new(ObjectStore::new(MemoryFileStore::new()));
		let want = ObjectId::<Sha256>::from_hex(&"0".repeat(Sha256::RAW_LEN * 2)).unwrap();
		let error = LocalPackFetcher::new(source)
			.fetch_pack(&destination, &[want], &[], &Deepen::default(), false)
			.await
			.expect_err("a mismatched source must be refused");
		assert!(format!("{error:#}").contains("destination uses"));
	}
}
