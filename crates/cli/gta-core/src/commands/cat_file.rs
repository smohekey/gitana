use std::io::Write;
use std::path::Path;

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, parse_tree};
use gitana_repository::Repository;

use crate::dispatch::{self, ObjectCommand};

/// Show an object's type, size, or pretty-printed content.
pub async fn run(
	cwd: &Path,
	show_type: bool,
	show_size: bool,
	pretty: bool,
	object: &str,
) -> Result<()> {
	dispatch::on_object(
		cwd,
		object,
		CatFile {
			show_type,
			show_size,
			pretty,
		},
	)
	.await
}

struct CatFile {
	show_type: bool,
	show_size: bool,
	pretty: bool,
}

impl ObjectCommand for CatFile {
	async fn run<H: HashAlgorithm>(
		self,
		repo: Repository<Backend, H>,
		oid: ObjectId<H>,
	) -> Result<()> {
		let (kind, payload) = repo.objects().read_object(&oid).await?;

		if self.show_type {
			println!("{}", kind.as_str());
		} else if self.show_size {
			println!("{}", payload.len());
		} else if self.pretty {
			pretty_print::<H>(kind, &payload)?;
		} else {
			bail!("one of -t, -s, -p is required");
		}
		Ok(())
	}
}

fn pretty_print<H: HashAlgorithm>(kind: ObjectKind, payload: &[u8]) -> Result<()> {
	match kind {
		ObjectKind::Tree => {
			let mut out = String::new();
			for entry in parse_tree::<H>(payload)? {
				let object_type = if entry.mode == "40000" {
					"tree"
				} else {
					"blob"
				};
				out.push_str(&format!(
					"{:0>6} {} {}\t{}\n",
					entry.mode, object_type, entry.id, entry.name
				));
			}
			print!("{out}");
		}
		// Blob, commit, and tag print their raw canonical payload.
		_ => std::io::stdout().write_all(payload)?,
	}
	Ok(())
}
