use std::path::{Path, PathBuf};

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_porcelain::Identity;
use gitana_repository::{ReflogIntent, Repository};

use crate::dispatch::{self, RepoCommand};
use crate::identity::CliIdentity;
use crate::signer::{self, LazyCliSigner};

/// List tags, or create one at `target` (default `HEAD`).
///
/// `annotate`/`sign`/`message` select the *kind*: a bare name (none of these) makes a lightweight tag,
/// as before; any of them makes an annotated tag object (`-m` and `-s` both imply `-a`). `sign`/
/// `no_sign` are the explicit `-s`/`--no-sign` flags; absent, an annotated tag follows git config
/// `tag.gpgSign`. `signing_key` overrides `user.signingkey` when signing.
#[allow(clippy::too_many_arguments)]
pub async fn run(
	cwd: &Path,
	name: Option<String>,
	target: Option<String>,
	annotate: bool,
	sign: bool,
	no_sign: bool,
	message: Option<String>,
	signing_key: Option<PathBuf>,
) -> Result<()> {
	dispatch::on_repo(
		cwd,
		Tag {
			name,
			target,
			annotate,
			sign,
			no_sign,
			message,
			signing_key,
			cwd: cwd.to_path_buf(),
		},
	)
	.await
}

struct Tag {
	name: Option<String>,
	target: Option<String>,
	annotate: bool,
	sign: bool,
	no_sign: bool,
	message: Option<String>,
	signing_key: Option<PathBuf>,
	/// The effective working directory, for resolving a relative signing-key path (`-C`).
	cwd: PathBuf,
}

impl RepoCommand for Tag {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let Some(name) = self.name.clone() else {
			for (name, _) in repo.refs().list("refs/tags/").await? {
				println!("{}", name.strip_prefix("refs/tags/").unwrap_or(&name));
			}
			return Ok(());
		};

		let full = format!("refs/tags/{name}");
		if repo.refs().resolve(&full).await?.is_some() {
			bail!("tag '{name}' already exists");
		}
		let oid = repo
			.rev_parse(self.target.as_deref().unwrap_or("HEAD"))
			.await?;

		// `-a`/`-m`/`-s` (or `tag.gpgSign` requesting a signature) make an annotated tag object;
		// otherwise the ref points straight at `oid` — a lightweight tag.
		let tag_oid = if self.is_annotated(&repo).await? {
			self.create_annotated(&repo, &name, oid).await?
		} else {
			oid
		};
		// git does not keep a reflog for tags (they are immutable), so the ref move opts out.
		repo
			.refs()
			.update_ref(&full, tag_oid, None, ReflogIntent::Skip)
			.await?;
		Ok(())
	}
}

impl Tag {
	/// Whether this invocation creates an annotated tag object rather than a lightweight ref: any of
	/// `-a`/`-m`/`-s`, or git config `tag.gpgSign` requesting a signature. A bare `gta tag <name>` stays
	/// lightweight even under `tag.gpgSign` — as git does.
	async fn is_annotated<H: HashAlgorithm>(&self, repo: &Repository<Backend, H>) -> Result<bool> {
		Ok(
			self.annotate
				|| self.sign
				|| self.message.is_some()
				|| (!self.no_sign && signer::config_requests_tag_signing(repo).await?),
		)
	}

	/// Build the annotated (optionally signed) tag object and return its id. Requires a message — git
	/// would open an editor, which the CLI has no equivalent for.
	async fn create_annotated<H: HashAlgorithm>(
		self,
		repo: &Repository<Backend, H>,
		name: &str,
		oid: ObjectId<H>,
	) -> Result<ObjectId<H>> {
		let Some(message) = self.message.as_deref() else {
			bail!("annotated tag requires a message: pass -m <msg>");
		};
		// git records the *committer* identity as the tagger.
		let tagger = CliIdentity::new(repo).committer().await?;

		if self.should_sign(repo).await? {
			let signer = LazyCliSigner::new(repo, self.signing_key, self.cwd);
			gitana_porcelain::tag_signed(repo, oid, name, &tagger, message, &signer).await
		} else {
			gitana_porcelain::tag(repo, oid, name, &tagger, message).await
		}
	}

	/// Whether the annotated tag should be signed: `--no-sign` wins (off), then `-s`/`--sign`, then git
	/// config `tag.gpgSign`. Fails *closed*: a config read/parse error propagates rather than dropping
	/// to unsigned. The signing format (`gpg.format`) and key are resolved by the signer, lazily.
	async fn should_sign<H: HashAlgorithm>(&self, repo: &Repository<Backend, H>) -> Result<bool> {
		if self.no_sign {
			return Ok(false);
		}
		if self.sign {
			return Ok(true);
		}
		signer::config_requests_tag_signing(repo).await
	}
}
