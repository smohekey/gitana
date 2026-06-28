use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use gitana_object::{ObjectId, ObjectKind};

use crate::repo;

/// Compute (and with `-w` write) the id of an object read from a file or stdin.
pub async fn run(
	cwd: &Path,
	kind: &str,
	write: bool,
	stdin: bool,
	file: Option<PathBuf>,
) -> Result<()> {
	let kind =
		ObjectKind::from_wire(kind.as_bytes()).map_err(|_| anyhow!("invalid object type: {kind}"))?;

	let content = if stdin {
		let mut buf = Vec::new();
		std::io::stdin().read_to_end(&mut buf)?;
		buf
	} else {
		let file = file.context("a file path or --stdin is required")?;
		std::fs::read(cwd.join(file))?
	};

	let oid = if write {
		repo::open_here(cwd)?
			.objects()
			.write_object(kind, &content)
			.await?
	} else {
		ObjectId::compute(kind, &content)
	};
	println!("{oid}");
	Ok(())
}
