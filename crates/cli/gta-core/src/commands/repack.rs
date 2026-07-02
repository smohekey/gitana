use std::path::Path;

use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::Backend;
use crate::dispatch::{self, RepoCommand};

/// Consolidate all stored objects (loose + existing packs) into a single new pack, removing
/// the now-redundant loose objects and old packs.
pub async fn run(cwd: &Path) -> Result<()> {
	dispatch::on_repo(cwd, Repack).await
}

struct Repack;

impl RepoCommand for Repack {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		match repo.objects().repack().await? {
			Some(report) => println!(
				"Packed {} objects into 1 pack (removed {} pack(s), {} loose object(s)).",
				report.packed_objects, report.packs_removed, report.loose_removed
			),
			None => println!("Nothing to repack."),
		}
		Ok(())
	}
}
