use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, ObjectKind, parse_tree};
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// List a tree's entries (`-r` recurses, listing blobs).
pub async fn run(cwd: &Path, recursive: bool, treeish: &str) -> Result<()> {
	dispatch::on_repo(cwd, LsTree { recursive, treeish }).await
}

struct LsTree<'a> {
	recursive: bool,
	treeish: &'a str,
}

impl RepoCommand for LsTree<'_> {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let oid = repo.rev_parse(self.treeish).await?;
		let (kind, _) = repo.objects().read_object(&oid).await?;
		let tree = match kind {
			ObjectKind::Commit => repo.commit_tree(oid).await?,
			ObjectKind::Tree => oid,
			other => bail!("{oid} is a {}, not a tree", other.as_str()),
		};

		if self.recursive {
			for (path, mode, id) in repo.read_tree(tree).await? {
				println!("{:0>6} blob {id}\t{path}", mode);
			}
		} else {
			let (_, payload) = repo.objects().read_object(&tree).await?;
			for entry in parse_tree::<H>(&payload)? {
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
}
