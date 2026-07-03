use std::path::Path;

use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::Backend;
use crate::dispatch::{self, RepoCommand};

/// Consolidate stored objects into size-bounded packs (`pack.packSizeLimit`). By default a full
/// repack of every object; with `geometric`, an incremental repack that keeps the large packs and
/// rolls only the small packs + loose objects into new ones.
pub async fn run(cwd: &Path, geometric: bool) -> Result<()> {
	dispatch::on_repo(cwd, Repack { geometric }).await
}

struct Repack {
	geometric: bool,
}

impl RepoCommand for Repack {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let max_pack_size = repo.pack_size_limit().await?;
		let report = if self.geometric {
			repo.objects().repack_geometric(max_pack_size, 2).await?
		} else {
			repo.objects().repack(max_pack_size).await?
		};
		match report {
			Some(report) => println!(
				"Packed {} objects into {} pack(s) (kept {} pack(s), removed {} pack(s), {} loose object(s)).",
				report.packed_objects,
				report.packs_written,
				report.packs_kept,
				report.packs_removed,
				report.loose_removed,
			),
			None => println!("Nothing to repack."),
		}
		Ok(())
	}
}
