use std::io::Write;
use std::path::Path;

use crate::{Backend, PathQuoteMode};
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
	object: &[u8],
	quote_path: PathQuoteMode,
) -> Result<()> {
	dispatch::on_object(
		cwd,
		object,
		CatFile {
			show_type,
			show_size,
			pretty,
			quote_path,
		},
	)
	.await
}

struct CatFile {
	show_type: bool,
	show_size: bool,
	pretty: bool,
	quote_path: PathQuoteMode,
}

impl ObjectCommand for CatFile {
	async fn run<H: HashAlgorithm>(
		self,
		repo: Repository<Backend, H>,
		oid: ObjectId<H>,
	) -> Result<()> {
		let config = repo.effective_config().await?;
		let configured_quote_path = config
			.get_bool_validated("core", None, "quotepath")?
			.unwrap_or(true);
		let quote_non_ascii = match self.quote_path {
			PathQuoteMode::Config => configured_quote_path,
			PathQuoteMode::Always => true,
		};
		let (kind, payload) = repo.objects().read_object(&oid).await?;

		if self.show_type {
			println!("{}", kind.as_str());
		} else if self.show_size {
			println!("{}", payload.len());
		} else if self.pretty {
			pretty_print::<H>(kind, &payload, quote_non_ascii)?;
		} else {
			bail!("one of -t, -s, -p is required");
		}
		Ok(())
	}
}

fn pretty_print<H: HashAlgorithm>(
	kind: ObjectKind,
	payload: &[u8],
	quote_non_ascii: bool,
) -> Result<()> {
	match kind {
		ObjectKind::Tree => {
			let mut out = Vec::new();
			for entry in parse_tree::<H>(payload)? {
				let object_type = if entry.mode == "40000" {
					"tree"
				} else {
					"blob"
				};
				out.extend_from_slice(
					format!("{:0>6} {} {}\t", entry.mode, object_type, entry.id).as_bytes(),
				);
				out.extend_from_slice(&entry.name.render(quote_non_ascii));
				out.push(b'\n');
			}
			std::io::stdout().write_all(&out)?;
		}
		// Blob, commit, and tag print their raw canonical payload.
		_ => std::io::stdout().write_all(payload)?,
	}
	Ok(())
}
