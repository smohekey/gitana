use std::path::Path;

use anyhow::{Result, bail};

use crate::repo;

/// List tags, or create a lightweight tag `name` at `target` (default `HEAD`).
pub async fn run(cwd: &Path, name: Option<String>, target: Option<String>) -> Result<()> {
	let repo = repo::open_here(cwd)?;
	match name {
		None => {
			for (name, _) in repo.refs().list("refs/tags/").await? {
				println!("{}", name.strip_prefix("refs/tags/").unwrap_or(&name));
			}
			Ok(())
		}
		Some(name) => {
			let full = format!("refs/tags/{name}");
			if repo.refs().resolve(&full).await?.is_some() {
				bail!("tag '{name}' already exists");
			}
			let oid = repo.rev_parse(target.as_deref().unwrap_or("HEAD")).await?;
			repo.refs().update_ref(&full, oid, None).await?;
			Ok(())
		}
	}
}
