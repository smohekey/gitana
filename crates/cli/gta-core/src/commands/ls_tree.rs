use std::{io::Write, path::Path};

use crate::{Backend, PathQuoteMode};
use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, ObjectKind, parse_tree};
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};

/// List a tree's entries (`-r` recurses, listing blobs).
pub async fn run(
	cwd: &Path,
	recursive: bool,
	treeish: &[u8],
	quote_path: PathQuoteMode,
) -> Result<()> {
	dispatch::on_repo(
		cwd,
		LsTree {
			recursive,
			treeish,
			quote_path,
		},
	)
	.await
}

struct LsTree<'a> {
	recursive: bool,
	treeish: &'a [u8],
	quote_path: PathQuoteMode,
}

impl RepoCommand for LsTree<'_> {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let config = repo.effective_config().await?;
		let configured_quote_path = config
			.get_bool_validated("core", None, "quotepath")?
			.unwrap_or(true);
		let quote_non_ascii = match self.quote_path {
			PathQuoteMode::Config => configured_quote_path,
			PathQuoteMode::Always => true,
		};
		let oid = repo.rev_parse(self.treeish).await?;
		let (kind, _) = repo.objects().read_object(&oid).await?;
		let tree = match kind {
			ObjectKind::Commit => repo.commit_tree(oid).await?,
			ObjectKind::Tree => oid,
			other => bail!("{oid} is a {}, not a tree", other.as_str()),
		};

		let mut out = Vec::new();
		if self.recursive {
			for (path, mode, id) in repo.read_tree_raw(tree).await? {
				out.extend_from_slice(format!("{:0>6} blob {id}\t", mode).as_bytes());
				out.extend_from_slice(&path.render_with_affixes(b"", b"", quote_non_ascii));
				out.push(b'\n');
			}
		} else {
			let (_, payload) = repo.objects().read_object(&tree).await?;
			for entry in parse_tree::<H>(&payload)? {
				let object_type = if entry.mode == "40000" {
					"tree"
				} else {
					"blob"
				};
				out
					.extend_from_slice(format!("{:0>6} {object_type} {}\t", entry.mode, entry.id).as_bytes());
				out.extend_from_slice(&entry.name.render(quote_non_ascii));
				out.push(b'\n');
			}
		}
		let mut stdout = std::io::stdout().lock();
		stdout.write_all(&out)?;
		stdout.flush()?;
		Ok(())
	}
}
