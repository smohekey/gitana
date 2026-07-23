//! A high-level, credential-injected clone over the native HTTP transport.
//!
//! [`clone_url`] is the orchestration [`gitana_porcelain::clone`] leaves to its caller: parse the URL,
//! build the authenticating transport from a caller-supplied [`CredentialProvider`], create the git
//! directory skeleton, open the repository and working tree, and run the porcelain clone. It exists so
//! a programmatic consumer — one holding credentials in its own store rather than in git config,
//! askpass helpers, or a terminal — can clone with a single call, without reaching into the transport
//! and storage plumbing the CLI (`gta clone`) assembles by hand.
//!
//! This is deliberately narrower than `gta clone`: no `insteadOf` URL rewriting from ambient git
//! config, no interactive prompting, no reflog identity. Credentials come only from the provider, and a
//! headless clone writes no `clone: from …` reflog entry. Only the HTTP(S) transport is supported;
//! an SSH URL is refused.

use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_file_store_local::{CapWorkDir, WorktreeFileStore};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_porcelain::Deepen;
use gitana_remote::{
	self as remote, AuthTransport, Connection, CredentialProvider, HttpConnection, RemoteUrl,
	ReqwestTransport,
};
use gitana_repository::Repository;

/// Clone the repository at `url` into `destination`, authenticating through `credentials`.
///
/// `url` must be an HTTP(S) remote; an SSH URL is [`CloneError::UnsupportedTransport`]. `destination`
/// must be empty or absent — a non-empty directory is [`CloneError::DestinationNotEmpty`], mirroring
/// git's refusal to clone over existing content. The repository is created in whatever object format
/// the remote advertises. `deepen` requests a shallow clone (an empty [`Deepen`] is a full clone).
///
/// Credentials are resolved only through `credentials`: an anonymous provider clones a public
/// repository, and a provider returning a token authenticates against a private one. No credential is
/// read from git config, an askpass helper, or a terminal.
pub async fn clone_url(
	url: &str,
	destination: &Path,
	credentials: impl CredentialProvider,
	deepen: &Deepen,
) -> Result<(), CloneError> {
	let remote = RemoteUrl::parse(url).map_err(|source| CloneError::Url {
		url: url.to_owned(),
		source: source.into(),
	})?;
	let origin = match remote {
		RemoteUrl::Http(origin) => origin,
		RemoteUrl::Ssh(_) => return Err(CloneError::UnsupportedTransport),
	};
	ensure_empty_destination(destination)?;
	let git_dir = destination.join(".git");
	create_skeleton(&git_dir)?;

	// One authenticating transport carries the credential through both the advertisement GET and the
	// pack POST. A full `user:pass` in the URL seeds the challenge retry; the provider answers a 401.
	let transport = AuthTransport::with_userinfo(
		ReqwestTransport::new(),
		credentials,
		origin.url.clone(),
		origin.username.clone(),
		origin.password.clone(),
	);
	let advertisement = remote::fetch_advertisement(&transport, &origin, "git-upload-pack")
		.await
		.map_err(|source| CloneError::Advertisement {
			url: origin.url.clone(),
			source: source.into(),
		})?;
	let kind =
		remote::negotiated_kind(&advertisement).map_err(|source| CloneError::Advertisement {
			url: origin.url.clone(),
			source: source.into(),
		})?;
	let mut connection = HttpConnection::new(
		&transport,
		origin.upload_pack(),
		remote::UPLOAD_PACK_REQUEST,
		advertisement,
	);

	// The origin URL to persist as `remote.origin.url`: userinfo-free, so no credential reaches config.
	let persist_url = origin.url.clone();
	match kind {
		HashKind::Sha1 => {
			clone_into::<Sha1>(&mut connection, &git_dir, destination, deepen, &persist_url).await
		}
		HashKind::Sha256 => {
			clone_into::<Sha256>(&mut connection, &git_dir, destination, deepen, &persist_url).await
		}
	}
}

async fn clone_into<H: HashAlgorithm>(
	connection: &mut impl Connection,
	git_dir: &Path,
	destination: &Path,
	deepen: &Deepen,
	persist_url: &str,
) -> Result<(), CloneError> {
	let repository = open_repository::<H>(git_dir)?;
	let work = open_work_dir(destination)?;
	// A headless clone writes no reflog identity (`reflog: None`), as gitana's in-component clone does.
	gitana_porcelain::clone(connection, repository, work, deepen, None, persist_url)
		.await
		.map_err(|source| CloneError::Clone {
			destination: destination.to_owned(),
			source: source.into(),
		})
}

/// Open the freshly-skeletoned repository at `git_dir`. An ordinary (non-worktree) repository's common
/// directory is its git directory, so both capabilities open the same path.
fn open_repository<H: HashAlgorithm>(
	git_dir: &Path,
) -> Result<Repository<WorktreeFileStore, H>, CloneError> {
	let common = Dir::open_ambient_dir(git_dir, ambient_authority()).map_err(|source| {
		CloneError::Destination {
			path: git_dir.to_owned(),
			source,
		}
	})?;
	let git = Dir::open_ambient_dir(git_dir, ambient_authority()).map_err(|source| {
		CloneError::Destination {
			path: git_dir.to_owned(),
			source,
		}
	})?;
	Ok(Repository::new(ObjectStore::new(WorktreeFileStore::new(
		common, git,
	))))
}

fn open_work_dir(destination: &Path) -> Result<CapWorkDir, CloneError> {
	let dir = Dir::open_ambient_dir(destination, ambient_authority()).map_err(|source| {
		CloneError::Destination {
			path: destination.to_owned(),
			source,
		}
	})?;
	Ok(CapWorkDir::from_dir(dir))
}

/// Refuse a destination that exists and holds any entry, and create it (and its parents) otherwise.
fn ensure_empty_destination(destination: &Path) -> Result<(), CloneError> {
	match std::fs::read_dir(destination) {
		Ok(mut entries) => {
			if entries.next().is_some() {
				return Err(CloneError::DestinationNotEmpty(destination.to_owned()));
			}
			Ok(())
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			std::fs::create_dir_all(destination).map_err(|source| CloneError::Destination {
				path: destination.to_owned(),
				source,
			})
		}
		Err(source) => Err(CloneError::Destination {
			path: destination.to_owned(),
			source,
		}),
	}
}

/// Create the git-directory skeleton a fresh clone populates, like `init`.
fn create_skeleton(git_dir: &Path) -> Result<(), CloneError> {
	for sub in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		std::fs::create_dir_all(git_dir.join(sub)).map_err(|source| CloneError::Destination {
			path: git_dir.join(sub),
			source,
		})?;
	}
	Ok(())
}

/// Why a [`clone_url`] did not complete.
#[derive(Debug, thiserror::Error)]
pub enum CloneError {
	#[error("clone URL {url:?} is not a valid remote URL")]
	Url {
		url: String,
		#[source]
		source: Box<dyn std::error::Error + Send + Sync>,
	},
	#[error("only HTTP(S) clone is supported; SSH is not yet implemented")]
	UnsupportedTransport,
	#[error("clone destination {} exists and is not empty", .0.display())]
	DestinationNotEmpty(PathBuf),
	#[error("preparing clone destination {}", .path.display())]
	Destination {
		path: PathBuf,
		#[source]
		source: std::io::Error,
	},
	#[error("fetching the ref advertisement from {url:?}")]
	Advertisement {
		url: String,
		#[source]
		source: Box<dyn std::error::Error + Send + Sync>,
	},
	#[error("cloning into {}", .destination.display())]
	Clone {
		destination: PathBuf,
		#[source]
		source: Box<dyn std::error::Error + Send + Sync>,
	},
}

#[cfg(test)]
mod tests {
	use gitana_remote::{Credential, CredentialRequest, Filled};

	use super::*;

	/// A provider that never supplies a credential — an anonymous clone.
	struct Anonymous;

	impl CredentialProvider for Anonymous {
		async fn fill(&self, _request: &CredentialRequest) -> anyhow::Result<Option<Filled>> {
			Ok(None)
		}

		async fn approve(
			&self,
			_request: &CredentialRequest,
			_credential: &Credential,
		) -> anyhow::Result<()> {
			Ok(())
		}

		async fn reject(
			&self,
			_request: &CredentialRequest,
			_credential: &Credential,
		) -> anyhow::Result<()> {
			Ok(())
		}
	}

	#[tokio::test]
	async fn an_ssh_url_is_unsupported() {
		let temp = tempfile::tempdir().unwrap();
		let error = clone_url(
			"ssh://git@example.invalid/repo.git",
			&temp.path().join("dest"),
			Anonymous,
			&Deepen::default(),
		)
		.await
		.expect_err("SSH must be refused");
		assert!(matches!(error, CloneError::UnsupportedTransport));
	}

	#[tokio::test]
	async fn a_malformed_url_is_reported() {
		let temp = tempfile::tempdir().unwrap();
		let error = clone_url(
			"not a url",
			&temp.path().join("dest"),
			Anonymous,
			&Deepen::default(),
		)
		.await
		.expect_err("a malformed URL must be refused");
		assert!(matches!(error, CloneError::Url { .. }));
	}

	#[tokio::test]
	async fn a_non_empty_destination_is_refused_before_any_network_access() {
		let temp = tempfile::tempdir().unwrap();
		let destination = temp.path().join("occupied");
		std::fs::create_dir(&destination).unwrap();
		std::fs::write(destination.join("stray"), b"x").unwrap();

		// The URL is valid and unreachable; the destination check must fail first, so no request is made.
		let error = clone_url(
			"https://example.invalid/repo.git",
			&destination,
			Anonymous,
			&Deepen::default(),
		)
		.await
		.expect_err("a non-empty destination must be refused");
		assert!(matches!(error, CloneError::DestinationNotEmpty(_)));
	}
}
