use std::path::PathBuf;

use anyhow::{Result, bail};
use gitana_object::{HashKind, Sha1, Sha256};

use crate::repo;

/// Create a git-compatible repository under `target/.git` in the `object_format` hash
/// format (`sha1` or `sha256`).
pub async fn run(target: PathBuf, object_format: &str) -> Result<()> {
	let kind = match object_format {
		"sha256" => HashKind::Sha256,
		"sha1" => HashKind::Sha1,
		other => bail!("unknown object format: {other} (expected sha1 or sha256)"),
	};

	let git_dir = target.join(".git");
	// The engine writes config + HEAD; the local profile creates the directory
	// skeleton git requires (objects/, refs/, info/).
	for sub in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		std::fs::create_dir_all(git_dir.join(sub))?;
	}

	// `init` is the one place a repository's hash is chosen rather than detected, so it
	// dispatches on the requested format and writes a config matching it.
	match kind {
		HashKind::Sha1 => {
			repo::open_generic::<Sha1>(&git_dir, &git_dir)?
				.init()
				.await?
		}
		HashKind::Sha256 => {
			repo::open_generic::<Sha256>(&git_dir, &git_dir)?
				.init()
				.await?
		}
	}

	println!(
		"Initialized empty Gitana repository in {}",
		git_dir.display()
	);
	Ok(())
}
