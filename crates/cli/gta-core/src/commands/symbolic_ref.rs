use std::path::Path;

use anyhow::{Result, bail};

use crate::repo;

/// Read or set a symbolic ref (`name [target]`).
pub async fn run(cwd: &Path, name: &str, target: Option<String>) -> Result<()> {
	let repo = repo::open_here(cwd)?;
	match target {
		Some(target) => repo.refs().set_symbolic(name, &target).await?,
		None => match repo.refs().read_symbolic(name).await? {
			Some(target) => println!("{target}"),
			None => bail!("{name} is not a symbolic ref"),
		},
	}
	Ok(())
}
