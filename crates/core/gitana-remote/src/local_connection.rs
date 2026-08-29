//! An in-process upload-pack connection over an injected local repository.

use anyhow::Result;
use gitana_file_store::FileStore;
use gitana_git_http::{ProtocolVersion, Service, advertise, upload_pack_v0};
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::Connection;

/// A clone connection served from `repository` without network or subprocess access.
pub struct LocalConnection<F: FileStore, H: HashAlgorithm> {
	repository: Repository<F, H>,
	advertisement: Vec<u8>,
}

impl<F: FileStore, H: HashAlgorithm> LocalConnection<F, H> {
	/// Build the source repository's protocol-v0 advertisement.
	pub async fn open(repository: Repository<F, H>) -> Result<Self> {
		let advertisement =
			advertise(&repository, Service::UploadPack, ProtocolVersion::V0, None).await?;
		Ok(Self {
			repository,
			advertisement,
		})
	}
}

impl<F: FileStore, H: HashAlgorithm> Connection for LocalConnection<F, H> {
	fn advertisement(&self) -> &[u8] {
		&self.advertisement
	}

	async fn exchange(&mut self, body: Vec<u8>) -> Result<Vec<u8>> {
		Ok(upload_pack_v0(&self.repository, &body).await?)
	}

	async fn finish(&mut self) -> Result<()> {
		Ok(())
	}
}
