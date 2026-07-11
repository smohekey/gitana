use std::io::Read;
use std::path::{Path, PathBuf};

use crate::Backend;
use anyhow::{Context, Result, anyhow};
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind};
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};
use crate::{Oid, repo};

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

	// Use the containing repository's hash format for the id — so even a compute-only run
	// matches `git hash-object` in a sha1 repo. Outside any repository, `-w` has nowhere to
	// write (propagate the discovery error), and a bare compute falls back to sha256 (the
	// format `gta init` defaults to).
	match repo::discover(cwd).await {
		Ok(_) => {
			dispatch::on_repo(
				cwd,
				HashObject {
					kind,
					content,
					write,
				},
			)
			.await
		}
		// Only a genuine absence falls back to a bare (sha256) compute. Corrupt or inaccessible
		// repository metadata — a broken `.git` file, a malformed `commondir` — is a real error even for
		// a compute-only run: a `-w` has nowhere to write, and a plain compute could otherwise emit the
		// wrong-format id for the checkout it is standing in.
		Err(repo::DiscoveryError::NotFound { .. }) if !write => {
			println!("{}", Oid::compute(kind, &content));
			Ok(())
		}
		Err(error) => Err(error.into()),
	}
}

struct HashObject {
	kind: ObjectKind,
	content: Vec<u8>,
	write: bool,
}

impl RepoCommand for HashObject {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let oid = if self.write {
			repo
				.objects()
				.write_object(self.kind, &self.content)
				.await?
		} else {
			ObjectId::<H>::compute(self.kind, &self.content)
		};
		println!("{oid}");
		Ok(())
	}
}
