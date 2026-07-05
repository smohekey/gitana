//! `gta trust` — manage the repository's trust root (the signed `refs/gitana/trust` chain). `init`
//! bootstraps a self-signed root enrolling the signing key; `list` shows the current policy and
//! enrolled key fingerprints.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use gitana_object::HashAlgorithm;
use gitana_porcelain::{TRUST_REF, trust_init, trust_list};
use gitana_repository::Repository;
use gitana_trust::{Policy, TrustRoot};

use crate::Backend;
use crate::dispatch::{self, RepoCommand};
use crate::identity::CliIdentity;
use crate::signer::CliSigner;

/// A `trust` sub-command.
pub enum Action {
	/// Bootstrap the trust root, enrolling the signing key under `policy`.
	Init {
		policy: String,
		signing_key: Option<PathBuf>,
		break_glass: bool,
	},
	/// Show the current policy and enrolled key fingerprints.
	List,
}

/// Run a `trust` sub-command in the repository containing `cwd`.
pub async fn run(cwd: &Path, action: Action) -> Result<()> {
	dispatch::on_repo(
		cwd,
		Trust {
			action,
			cwd: cwd.to_path_buf(),
		},
	)
	.await
}

struct Trust {
	action: Action,
	/// The effective working directory, for resolving a relative signing-key path (honors `-C`).
	cwd: PathBuf,
}

impl RepoCommand for Trust {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		match self.action {
			Action::Init {
				policy,
				signing_key,
				break_glass,
			} => {
				let policy = parse_policy(&policy)?;
				let signer = CliSigner::resolve(&repo, signing_key, &self.cwd).await?;
				let identity = CliIdentity::new(&repo);
				let pubkey = signer.public_line().await?;
				let tip = trust_init(&repo, policy, &pubkey, break_glass, &identity, &signer).await?;
				println!("Initialised trust root at {tip}");
				print_root(&repo).await
			}
			Action::List => print_root(&repo).await,
		}
	}
}

/// Print the current trust policy and enrolled key fingerprints, or a notice when trust is unset.
async fn print_root<H: HashAlgorithm>(repo: &Repository<Backend, H>) -> Result<()> {
	match trust_list(repo).await? {
		None => println!("No trust root configured ({TRUST_REF} is unset)."),
		Some(root) => print_trust_root(&root),
	}
	Ok(())
}

fn print_trust_root(root: &TrustRoot) {
	println!("policy: {}", policy_label(root.policy));
	println!("keys ({}):", root.keys.len());
	for key in &root.keys {
		println!("  {}", key.id());
	}
}

/// Parse the `--policy` value. Accepts git-trust's three policies; the default is `warn`.
fn parse_policy(value: &str) -> Result<Policy> {
	match value {
		"off" => Ok(Policy::Off),
		"warn" => Ok(Policy::Warn),
		"require" => Ok(Policy::Require),
		other => bail!("unknown policy `{other}` (expected off, warn, or require)"),
	}
}

fn policy_label(policy: Policy) -> &'static str {
	match policy {
		Policy::Off => "off",
		Policy::Warn => "warn",
		Policy::Require => "require",
	}
}
