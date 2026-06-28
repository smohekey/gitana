use std::io::Write;
use std::path::Path;

use anyhow::{Result, bail};
use gitana_object::{ObjectKind, parse_tree};

use crate::repo;

/// Show an object's type, size, or pretty-printed content.
pub async fn run(
	cwd: &Path,
	show_type: bool,
	show_size: bool,
	pretty: bool,
	object: &str,
) -> Result<()> {
	let repo = repo::open_here(cwd)?;
	let oid = repo.rev_parse(object).await?;
	let (kind, payload) = repo.objects().read_object(&oid).await?;

	if show_type {
		println!("{}", kind.as_str());
	} else if show_size {
		println!("{}", payload.len());
	} else if pretty {
		pretty_print(kind, &payload)?;
	} else {
		bail!("one of -t, -s, -p is required");
	}
	Ok(())
}

fn pretty_print(kind: ObjectKind, payload: &[u8]) -> Result<()> {
	match kind {
		ObjectKind::Tree => {
			let mut out = String::new();
			for entry in parse_tree(payload)? {
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
