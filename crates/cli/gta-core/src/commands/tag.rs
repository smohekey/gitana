use std::path::{Path, PathBuf};

use crate::Backend;
use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_porcelain::Identity;
use gitana_repository::Repository;

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
		repo.refs().update_ref(&full, tag_oid, None).await?;
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

		match self.signing_mode(repo).await? {
			SigningMode::Off => gitana_porcelain::tag(repo, oid, name, &tagger, message).await,
			mode => {
				let signer = LazyCliSigner::new(
					repo,
					self.signing_key,
					self.cwd,
					mode.require_explicit_ssh(),
				);
				gitana_porcelain::tag_signed(repo, oid, name, &tagger, message, &signer).await
			}
		}
	}

	/// Resolve how the annotated tag should sign: `--no-sign` wins (off), then `-s` (explicit — assumes
	/// ssh on an unset `gpg.format`), then git config `tag.gpgSign` (config — requires an explicit
	/// `gpg.format=ssh`). Fails *closed*: a config read/parse error propagates rather than dropping to
	/// unsigned.
	async fn signing_mode<H: HashAlgorithm>(
		&self,
		repo: &Repository<Backend, H>,
	) -> Result<SigningMode> {
		if self.no_sign {
			return Ok(SigningMode::Off);
		}
		if self.sign {
			return Ok(SigningMode::Explicit);
		}
		Ok(if signer::config_requests_tag_signing(repo).await? {
			SigningMode::Config
		} else {
			SigningMode::Off
		})
	}
}

/// How `gta tag` should sign an annotated tag — mirrors `gta commit`'s modes.
enum SigningMode {
	/// Do not sign.
	Off,
	/// `-s`/`--sign`: sign, assuming `gpg.format=ssh` when unset.
	Explicit,
	/// `tag.gpgSign`: sign, requiring an explicit `gpg.format=ssh`.
	Config,
}

impl SigningMode {
	/// Whether an unset `gpg.format` is rejected (config-driven) rather than assumed `ssh` (explicit
	/// `-s`). Only meaningful for the signing modes.
	fn require_explicit_ssh(&self) -> bool {
		matches!(self, SigningMode::Config)
	}
}
