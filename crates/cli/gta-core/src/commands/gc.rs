use std::path::Path;

use anyhow::Result;
use gitana_object::HashAlgorithm;
use gitana_worktree::WorkTree;

use crate::Backend;
use crate::dispatch::{self, WorkTreeCommand};

/// Delete unreachable loose objects (prune) then incrementally (geometrically) repack.
pub async fn run(cwd: &Path) -> Result<()> {
	dispatch::on_worktree(cwd, Gc).await
}

struct Gc;

impl WorkTreeCommand for Gc {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, H>,
		_prefix: String,
	) -> Result<()> {
		let (prune, repack, bitmap) = gitana_porcelain::gc(&worktree).await?;
		println!("Pruned {} unreachable object(s).", prune.pruned);
		match repack {
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
		if let Some(report) = bitmap {
			println!(
				"Wrote a reachability bitmap over {} commit(s) across {} pack(s).",
				report.bitmapped_commits, report.packs,
			);
		}
		Ok(())
	}
}
