use std::path::Path;

use anyhow::{Result, bail};
use gitana_object::{ObjectKind, parse_tree};

use crate::repo;

/// List a tree's entries (`-r` recurses, listing blobs).
pub async fn run(cwd: &Path, recursive: bool, treeish: &str) -> Result<()> {
	let repo = repo::open_here(cwd)?;
	let oid = repo.rev_parse(treeish).await?;
	let (kind, _) = repo.objects().read_object(&oid).await?;
	let tree = match kind {
		ObjectKind::Commit => repo.commit_tree(oid).await?,
		ObjectKind::Tree => oid,
		other => bail!("{oid} is a {}, not a tree", other.as_str()),
	};

	if recursive {
		for (path, mode, id) in repo.read_tree(tree).await? {
			println!("{:0>6} blob {id}\t{path}", mode);
		}
	} else {
		let (_, payload) = repo.objects().read_object(&tree).await?;
		for entry in parse_tree(&payload)? {
			let object_type = if entry.mode == "40000" {
				"tree"
			} else {
				"blob"
			};
			println!(
				"{:0>6} {object_type} {}\t{}",
				entry.mode, entry.id, entry.name
			);
		}
	}
	Ok(())
}
