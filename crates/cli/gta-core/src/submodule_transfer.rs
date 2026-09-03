use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow, bail};
use cap_std::fs::Dir;
use gitana_config::GitConfig;
use gitana_file_store::FileStore;
use gitana_file_store_local::LocalFileStore;
use gitana_git_http::Deepen;
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_object_store::ObjectStore;
use gitana_porcelain::{TagFetch, fetch, fetch_object, prepare_clone};
use gitana_remote::{
	AuthTransport, Connection, HttpConnection, HttpPackFetcher, LocalConnection, LocalPackFetcher,
	Origin, PackFetcher, ProtocolContext, RemoteUrl, ReqwestTransport, SshCommand, SshConnection,
	SshPackFetcher, SshRemote,
};
use gitana_repository::{Repository, detect_hash_kind};
use gitana_submodule::{
	FetchRepository, FetchSource, FetchedTransfer, PrepareRepository, PrepareSource,
	PreparedTransfer, RepositoryTransfer, SubmoduleObjectId, resolve_relative_url,
};

use crate::{
	Backend, CliCredentialProvider, CommandContext, dispatch, repo, ssh, transport_for, url_rewrite,
};

type CliHttpTransport = AuthTransport<ReqwestTransport, CliCredentialProvider>;

enum PreparedTransport {
	Http {
		origin: Origin,
		transport: CliHttpTransport,
		advertisement: Vec<u8>,
	},
	Ssh {
		remote: SshRemote,
		command: SshCommand,
		connection: SshConnection,
	},
	Local {
		files: Backend,
	},
}

pub(crate) struct PreparedSubmoduleSource {
	secret_urls: Vec<String>,
	module_path: String,
	descendant_url_base: Option<String>,
	persist_url: String,
	transport: PreparedTransport,
}

struct ResolvedSubmoduleSource {
	rewritten: String,
	remote: RemoteUrl,
	identity: String,
}

/// Native transport implementation injected into `gitana-submodule`.
pub(crate) struct SubmoduleTransfer<'a> {
	command: &'a CommandContext,
	worktree_root: &'a Path,
	module_base: GitConfig,
	credential_url_base: Option<String>,
	descendant_url_bases: Mutex<HashMap<String, String>>,
}

impl<'a> SubmoduleTransfer<'a> {
	pub(crate) fn new(
		command: &'a CommandContext,
		worktree_root: &'a Path,
		module_base: GitConfig,
		credential_url_base: Option<String>,
	) -> Self {
		Self {
			command,
			worktree_root,
			module_base,
			credential_url_base,
			descendant_url_bases: Mutex::new(HashMap::new()),
		}
	}

	/// Remove the private credential-bearing bases prepared for successfully populated children.
	/// The recursive command consumes these before dropping the level transfer; they are never added
	/// to a core report or durable recovery record.
	pub(crate) fn take_descendant_url_bases(&self) -> Result<HashMap<String, String>> {
		let mut bases = self
			.descendant_url_bases
			.lock()
			.map_err(|_| anyhow!("submodule descendant URL state is unavailable"))?;
		Ok(std::mem::take(&mut *bases))
	}

	fn private_relative_source_url(&self, request: &PrepareSource) -> Option<String> {
		let base = self.credential_url_base.as_deref()?;
		let declared = request.declared_url.as_deref()?;
		if !declared.starts_with("./") && !declared.starts_with("../") {
			return None;
		}
		resolve_relative_url(base, declared).ok()
	}

	fn transfer_secrets(&self, request: &PrepareSource, private_source: Option<&str>) -> Vec<String> {
		let mut secrets = vec![request.source_url.clone()];
		if let Some(base) = &self.credential_url_base {
			secrets.push(base.clone());
		}
		if let Some(private_source) = private_source {
			secrets.push(private_source.to_owned());
		}
		secrets
	}

	fn resolve_prepare_source(
		&self,
		request: &PrepareSource,
		private_source: Option<&str>,
	) -> Result<ResolvedSubmoduleSource> {
		let configured =
			Self::resolve_source(&request.config, self.worktree_root, &request.source_url)?;
		if gitana_remote::redact_password(&configured.rewritten) != configured.rewritten {
			return Ok(configured);
		}
		let Some(private_source) = private_source else {
			return Ok(configured);
		};
		let safe_private_source = gitana_remote::redact_password(private_source);
		let candidate =
			Self::resolve_source(&request.config, self.worktree_root, &safe_private_source)?;
		if gitana_remote::redact_password(&candidate.rewritten)
			!= gitana_remote::redact_password(&configured.rewritten)
		{
			// An effective per-module URL override remains authoritative. A relative declaration is
			// not permission to copy the clone credential onto a different rewritten endpoint.
			return Ok(configured);
		}
		let Some(rewritten) = inherit_http_userinfo(private_source, &candidate.rewritten) else {
			return Ok(configured);
		};
		Self::resolve_rewritten_source(self.worktree_root, rewritten)
	}

	fn resolve_source(
		config: &GitConfig,
		base: &Path,
		source_url: &str,
	) -> Result<ResolvedSubmoduleSource> {
		let rewritten = url_rewrite::rewrite_fetch_url(config, source_url)?;
		Self::resolve_rewritten_source(base, rewritten)
	}

	fn resolve_rewritten_source(base: &Path, rewritten: String) -> Result<ResolvedSubmoduleSource> {
		let remote = RemoteUrl::parse(&rewritten)?;
		let identity = match &remote {
			RemoteUrl::Local(path) => {
				let path = lexical_normalize(&resolve_local_path(base, path));
				path
					.to_str()
					.map(ToOwned::to_owned)
					.ok_or_else(|| anyhow!("local remote path is not valid UTF-8: {}", path.display()))?
			}
			RemoteUrl::Http(_) | RemoteUrl::Ssh(_) => gitana_remote::redact_password(&rewritten),
		};
		Ok(ResolvedSubmoduleSource {
			rewritten,
			remote,
			identity,
		})
	}

	fn resolve_fetch_source_inner(&self, source: &FetchSource) -> Result<ResolvedSubmoduleSource> {
		Self::resolve_source(&source.config, &source.worktree_dir, &source.source_url)
	}

	async fn prepare_source_inner(
		&self,
		request: PrepareSource,
		resolved: ResolvedSubmoduleSource,
		secret_urls: Vec<String>,
		lease: gitana_submodule::SubmoduleMutationLease,
	) -> Result<PreparedTransfer<PreparedSubmoduleSource>> {
		let config = &request.config;
		let ResolvedSubmoduleSource {
			rewritten,
			remote,
			identity,
		} = resolved;
		self
			.command
			.authorize(config, &remote, ProtocolContext::Recursive)?;
		// Initial module transfers can use a rewrite that exists only in the superproject's local
		// configuration. Existing-module fetches deliberately rebuild configuration from the module
		// repository instead, so persisting the declaration alias would make the successfully cloned
		// repository unusable on its next update. Keep the effective network endpoint while stripping
		// any password introduced by either the declaration or the rewrite rule.
		let network_persist_url = gitana_remote::redact_password(&rewritten);
		let descendant_url_base = (network_persist_url != rewritten).then_some(rewritten.clone());

		let (persist_url, transport) = match remote {
			RemoteUrl::Http(origin) => {
				let http = transport_for(config.clone(), &origin, self.command.cwd().to_path_buf())?;
				let advertisement =
					gitana_remote::fetch_advertisement(&http, &origin, "git-upload-pack").await?;
				gitana_remote::ensure_same_format(
					request.hash_kind,
					gitana_remote::negotiated_kind(&advertisement)?,
				)?;
				(
					network_persist_url,
					PreparedTransport::Http {
						origin,
						transport: http,
						advertisement,
					},
				)
			}
			RemoteUrl::Ssh(remote) => {
				let command = ssh::resolve_ssh_command(config)?;
				let connection =
					SshConnection::open(&remote, "git-upload-pack", &command, self.command.cwd()).await?;
				gitana_remote::ensure_same_format(
					request.hash_kind,
					gitana_remote::negotiated_kind(connection.advertisement())?,
				)?;
				(
					network_persist_url,
					PreparedTransport::Ssh {
						remote,
						command,
						connection,
					},
				)
			}
			RemoteUrl::Local(path) => {
				let source_path = resolve_local_path(self.worktree_root, &path);
				let source_layout = repo::inspect_root(&source_path).await?;
				let source_identity = repo::capture_repository_layout_identity(&source_layout)?;
				let (source_setup, common, git) =
					repo::revalidated_local_source_setup(&source_layout, source_identity, Some(&lease))
						.await?;
				let persist_url = if Path::new(&path).is_relative() || rewritten != request.source_url {
					repo::local_source_url(&source_layout)?
				} else {
					request.persist_url
				};
				let files = Backend::new(common, git);
				let source_kind = detect_hash_kind(&files).await?;
				gitana_remote::ensure_same_format(request.hash_kind, source_kind)?;
				drop(source_setup);
				(persist_url, PreparedTransport::Local { files })
			}
		};
		Ok(PreparedTransfer {
			source: PreparedSubmoduleSource {
				secret_urls,
				module_path: request.module_path,
				descendant_url_base,
				persist_url,
				transport,
			},
			resolved_source: identity,
		})
	}

	async fn populate_prepared_inner(
		&self,
		source: PreparedSubmoduleSource,
		request: PrepareRepository,
	) -> Result<()> {
		match request.hash_kind {
			HashKind::Sha1 => self.populate_prepared_typed::<Sha1>(source, request).await,
			HashKind::Sha256 => {
				self
					.populate_prepared_typed::<Sha256>(source, request)
					.await
			}
		}
	}

	async fn populate_prepared_typed<H: HashAlgorithm>(
		&self,
		source: PreparedSubmoduleSource,
		request: PrepareRepository,
	) -> Result<()> {
		let recorded = typed_oid::<H>(&request.recorded)?;
		let repository = create_repository::<H>(request.git_dir, self.module_base.clone()).await?;
		let PreparedSubmoduleSource {
			module_path,
			descendant_url_base,
			persist_url,
			transport,
			..
		} = source;

		match transport {
			PreparedTransport::Http {
				origin,
				transport,
				advertisement,
			} => {
				let mut connection = HttpConnection::new(
					&transport,
					origin.upload_pack(),
					gitana_remote::UPLOAD_PACK_REQUEST,
					advertisement,
				);
				prepare_clone(
					&mut connection,
					&repository,
					&Deepen::default(),
					None,
					&persist_url,
				)
				.await?;
				if !repository.objects().exists_object(&recorded).await? {
					let mut fetcher = HttpPackFetcher::new(&transport, &origin);
					fetch_object(&mut fetcher, &repository, recorded).await?;
				}
			}
			PreparedTransport::Ssh {
				remote,
				command,
				mut connection,
			} => {
				prepare_clone(
					&mut connection,
					&repository,
					&Deepen::default(),
					None,
					&persist_url,
				)
				.await?;
				if !repository.objects().exists_object(&recorded).await? {
					let connection =
						SshConnection::open(&remote, "git-upload-pack", &command, self.command.cwd()).await?;
					let mut fetcher = SshPackFetcher::new(connection);
					fetch_object(&mut fetcher, &repository, recorded).await?;
				}
			}
			PreparedTransport::Local { files } => {
				let source: Repository<_, H> = Repository::new(ObjectStore::new(files.shared_handle()));
				let mut connection = LocalConnection::open(source).await?;
				prepare_clone(
					&mut connection,
					&repository,
					&Deepen::default(),
					None,
					&persist_url,
				)
				.await?;
				if !repository.objects().exists_object(&recorded).await? {
					let source: Repository<_, H> = Repository::new(ObjectStore::new(files.shared_handle()));
					let mut fetcher = LocalPackFetcher::new(source);
					fetch_object(&mut fetcher, &repository, recorded).await?;
				}
			}
		}
		if let Some(base) = descendant_url_base {
			self
				.descendant_url_bases
				.lock()
				.map_err(|_| anyhow!("submodule descendant URL state is unavailable"))?
				.insert(module_path, base);
		}
		Ok(())
	}

	async fn fetch_inner(
		&self,
		request: FetchRepository,
		lease: gitana_submodule::SubmoduleMutationLease,
	) -> Result<FetchedTransfer> {
		match request.hash_kind {
			HashKind::Sha1 => self.fetch_typed::<Sha1>(request, lease).await,
			HashKind::Sha256 => self.fetch_typed::<Sha256>(request, lease).await,
		}
	}

	async fn fetch_typed<H: HashAlgorithm>(
		&self,
		request: FetchRepository,
		lease: gitana_submodule::SubmoduleMutationLease,
	) -> Result<FetchedTransfer> {
		let FetchRepository {
			source,
			git_dir,
			recorded,
			..
		} = request;
		let FetchSource {
			source_url,
			worktree_dir,
			config,
		} = source;
		let repository = repository_from_dir::<H>(git_dir, config.clone());
		let ResolvedSubmoduleSource {
			remote, identity, ..
		} = Self::resolve_source(&config, &worktree_dir, &source_url)?;
		self
			.command
			.authorize(&config, &remote, ProtocolContext::Recursive)?;
		let recorded = typed_oid::<H>(&recorded)?;

		match remote {
			RemoteUrl::Http(origin) => {
				let http = transport_for(config, &origin, self.command.cwd().to_path_buf())?;
				let advertisement =
					gitana_remote::fetch_advertisement(&http, &origin, "git-upload-pack").await?;
				gitana_remote::ensure_same_format(
					kind::<H>(),
					gitana_remote::negotiated_kind(&advertisement)?,
				)?;
				let mut fetcher = HttpPackFetcher::new(&http, &origin);
				normal_then_exact(&mut fetcher, &repository, &advertisement, recorded).await?;
			}
			RemoteUrl::Ssh(remote) => {
				let command = ssh::resolve_ssh_command(&config)?;
				{
					let connection =
						SshConnection::open(&remote, "git-upload-pack", &command, self.command.cwd()).await?;
					gitana_remote::ensure_same_format(
						kind::<H>(),
						gitana_remote::negotiated_kind(connection.advertisement())?,
					)?;
					let advertisement = connection.advertisement().to_vec();
					let mut fetcher = SshPackFetcher::new(connection);
					normal_fetch(&mut fetcher, &repository, &advertisement).await?;
				}
				if !repository.objects().exists_object(&recorded).await? {
					let connection =
						SshConnection::open(&remote, "git-upload-pack", &command, self.command.cwd()).await?;
					gitana_remote::ensure_same_format(
						kind::<H>(),
						gitana_remote::negotiated_kind(connection.advertisement())?,
					)?;
					let mut fetcher = SshPackFetcher::new(connection);
					fetch_object(&mut fetcher, &repository, recorded).await?;
				}
			}
			RemoteUrl::Local(path) => {
				let source_path = resolve_local_path(&worktree_dir, &path);
				let source_layout = repo::inspect_root(&source_path).await?;
				let source_identity = repo::capture_repository_layout_identity(&source_layout)?;
				let (source_setup, common, git) =
					repo::revalidated_local_source_setup(&source_layout, source_identity, Some(&lease))
						.await?;
				let source_kind = dispatch::detect_algorithm_at(&common, &source_layout.common_dir).await?;
				gitana_remote::ensure_same_format(kind::<H>(), source_kind)?;
				let second_common = common.try_clone()?;
				let second_git = git.try_clone()?;
				let source = repo::open_generic_from_dirs::<H>(
					common,
					git,
					&source_layout.git_dir,
					&source_layout.common_dir,
				)
				.await?;
				let connection = LocalConnection::open(source).await?;
				let advertisement = connection.advertisement().to_vec();
				let source = repo::open_generic_from_dirs::<H>(
					second_common,
					second_git,
					&source_layout.git_dir,
					&source_layout.common_dir,
				)
				.await?;
				drop(source_setup);
				let mut fetcher = LocalPackFetcher::new(source);
				normal_then_exact(&mut fetcher, &repository, &advertisement, recorded).await?;
			}
		}
		Ok(FetchedTransfer {
			resolved_source: identity,
		})
	}
}

impl RepositoryTransfer for SubmoduleTransfer<'_> {
	type Error = TransferError;
	type PreparedSource = PreparedSubmoduleSource;

	fn resolve_source_identity(&self, request: &PrepareSource) -> Result<String, Self::Error> {
		let private_source = self.private_relative_source_url(request);
		let secrets = self.transfer_secrets(request, private_source.as_deref());
		self
			.resolve_prepare_source(request, private_source.as_deref())
			.map(|source| source.identity)
			.map_err(|error| TransferError::new(error, secrets))
	}

	fn resolve_fetch_source_identity(&self, source: &FetchSource) -> Result<String, Self::Error> {
		let secret = source.source_url.clone();
		self
			.resolve_fetch_source_inner(source)
			.map(|source| source.identity)
			.map_err(|error| TransferError::new(error, [secret]))
	}

	async fn prepare_source(
		&self,
		request: PrepareSource,
		lease: gitana_submodule::SubmoduleMutationLease,
	) -> Result<PreparedTransfer<Self::PreparedSource>, Self::Error> {
		let private_source = self.private_relative_source_url(&request);
		let mut secrets = self.transfer_secrets(&request, private_source.as_deref());
		let resolved = self
			.resolve_prepare_source(&request, private_source.as_deref())
			.map_err(|error| TransferError::new(error, secrets.clone()))?;
		secrets.push(resolved.rewritten.clone());
		self
			.prepare_source_inner(request, resolved, secrets.clone(), lease)
			.await
			.map_err(|error| TransferError::new(error, secrets))
	}

	async fn populate_prepared(
		&self,
		source: Self::PreparedSource,
		request: PrepareRepository,
	) -> Result<(), Self::Error> {
		let secrets = source.secret_urls.clone();
		let display = request.display_git_dir.clone();
		self
			.populate_prepared_inner(source, request)
			.await
			.with_context(|| format!("populating staged repository {}", display.display()))
			.map_err(|error| TransferError::new(error, secrets))
	}

	async fn fetch_recorded(
		&self,
		request: FetchRepository,
		lease: gitana_submodule::SubmoduleMutationLease,
	) -> Result<FetchedTransfer, Self::Error> {
		let secret = request.source.source_url.clone();
		let display = request.display_git_dir.clone();
		self
			.fetch_inner(request, lease)
			.await
			.with_context(|| format!("updating module repository {}", display.display()))
			.map_err(|error| TransferError::new(error, [secret]))
	}
}

fn inherit_http_userinfo(private_source: &str, rewritten: &str) -> Option<String> {
	let RemoteUrl::Http(private) = RemoteUrl::parse(private_source).ok()? else {
		return None;
	};
	let RemoteUrl::Http(target) = RemoteUrl::parse(rewritten).ok()? else {
		return None;
	};
	let username = private.username?;
	let password = private.password?;
	let (scheme, rest) = target.url.split_once("://")?;
	let inherited = format!(
		"{scheme}://{}:{}@{rest}",
		gitana_remote::percent_encode_userinfo(&username),
		gitana_remote::percent_encode_userinfo(&password)
	);
	(gitana_remote::redact_password(&inherited) == gitana_remote::redact_password(rewritten))
		.then_some(inherited)
}

#[derive(Debug)]
pub(crate) struct TransferError(String);

impl TransferError {
	fn new(error: anyhow::Error, secrets: impl IntoIterator<Item = String>) -> Self {
		let mut message = format!("{error:#}");
		for secret in secrets {
			message = message.replace(&secret, &gitana_remote::redact_password(&secret));
		}
		Self(message)
	}
}

impl std::fmt::Display for TransferError {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(&self.0)
	}
}

impl std::error::Error for TransferError {}

async fn normal_then_exact<F, H>(
	fetcher: &mut impl PackFetcher,
	repository: &gitana_repository::Repository<F, H>,
	advertisement: &[u8],
	recorded: ObjectId<H>,
) -> Result<()>
where
	F: gitana_file_store::FileStore,
	H: HashAlgorithm,
{
	normal_fetch(fetcher, repository, advertisement).await?;
	if !repository.objects().exists_object(&recorded).await? {
		fetch_object(fetcher, repository, recorded).await?;
	}
	Ok(())
}

async fn normal_fetch<F, H>(
	fetcher: &mut impl PackFetcher,
	repository: &gitana_repository::Repository<F, H>,
	advertisement: &[u8],
) -> Result<()>
where
	F: gitana_file_store::FileStore,
	H: HashAlgorithm,
{
	fetch(
		fetcher,
		repository,
		advertisement,
		false,
		TagFetch::Auto,
		false,
		&Deepen::default(),
		&[],
		None,
	)
	.await?;
	Ok(())
}

fn typed_oid<H: HashAlgorithm>(oid: &SubmoduleObjectId) -> Result<ObjectId<H>> {
	if oid.kind() != kind::<H>() {
		bail!("recorded object id has the wrong hash algorithm");
	}
	ObjectId::from_hex(oid.as_hex()).map_err(|error| anyhow!(error))
}

fn kind<H: HashAlgorithm>() -> HashKind {
	match H::NAME {
		"sha1" => HashKind::Sha1,
		"sha256" => HashKind::Sha256,
		_ => unreachable!("sealed hash algorithm"),
	}
}

fn resolve_local_path(cwd: &Path, path: &str) -> PathBuf {
	let path = PathBuf::from(path);
	if path.is_absolute() {
		path
	} else {
		cwd.join(path)
	}
}

fn lexical_normalize(path: &Path) -> PathBuf {
	let mut normalized = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				normalized.pop();
			}
			other => normalized.push(other.as_os_str()),
		}
	}
	normalized
}

async fn create_repository<H: HashAlgorithm>(
	git_dir: Dir,
	config: GitConfig,
) -> Result<Repository<LocalFileStore, H>> {
	let files = LocalFileStore::from_dir(git_dir);
	for subdir in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		files.create_dir_all(subdir).await?;
	}
	let mut repository = Repository::new(ObjectStore::new(files));
	repository.set_effective_config(config);
	Ok(repository)
}

fn repository_from_dir<H: HashAlgorithm>(
	git_dir: Dir,
	config: GitConfig,
) -> Repository<LocalFileStore, H> {
	let mut repository = Repository::new(ObjectStore::new(LocalFileStore::from_dir(git_dir)));
	repository.set_effective_config(config);
	repository
}

#[cfg(all(test, unix))]
mod tests {
	use cap_std::ambient_authority;
	use gitana_file_store::FileStore;

	use super::*;

	#[test]
	fn relative_clone_source_inherits_only_after_rewritten_endpoints_match() {
		let temporary = tempfile::tempdir().unwrap();
		let command = CommandContext::from_env(temporary.path().to_owned(), Vec::new());
		let transfer = SubmoduleTransfer::new(
			&command,
			temporary.path(),
			GitConfig::parse("").unwrap(),
			Some("https://alice:secret@example.invalid/team/root".to_owned()),
		);
		let matching = PrepareSource {
			module_path: "modules/child".to_owned(),
			declared_url: Some("../child".to_owned()),
			source_url: "https://alice@example.invalid/team/child".to_owned(),
			persist_url: "https://alice@example.invalid/team/child".to_owned(),
			hash_kind: HashKind::Sha256,
			config: GitConfig::parse("").unwrap(),
		};
		let private_source = transfer.private_relative_source_url(&matching);
		assert_eq!(
			transfer
				.resolve_prepare_source(&matching, private_source.as_deref())
				.unwrap()
				.rewritten,
			"https://alice:secret@example.invalid/team/child"
		);

		let overridden = PrepareSource {
			source_url: "https://other.invalid/child".to_owned(),
			..matching.clone()
		};
		let private_source = transfer.private_relative_source_url(&overridden);
		assert_eq!(
			transfer
				.resolve_prepare_source(&overridden, private_source.as_deref())
				.unwrap()
				.rewritten,
			"https://other.invalid/child",
			"an effective URL override must not inherit the clone credential"
		);

		let rewritten = PrepareSource {
			config: GitConfig::parse(
				"[url \"https://alice@example.invalid/mirror/\"]\n\tinsteadOf = https://alice@example.invalid/team/\n",
			)
			.unwrap(),
			..matching
		};
		let private_source = transfer.private_relative_source_url(&rewritten);
		assert_eq!(
			transfer
				.resolve_prepare_source(&rewritten, private_source.as_deref())
				.unwrap()
				.rewritten,
			"https://alice:secret@example.invalid/mirror/child",
			"the safe URL must be rewritten before the private userinfo is restored"
		);
	}

	#[test]
	fn transfer_errors_redact_every_private_url_form() {
		let base = "https://alice:root-secret@example.invalid/team/root".to_owned();
		let child = "https://alice:child-secret@example.invalid/team/child".to_owned();
		let error = TransferError::new(
			anyhow!("using {base} selected {child}"),
			[base.clone(), child.clone()],
		);
		let rendered = error.to_string();
		assert!(!rendered.contains("root-secret"));
		assert!(!rendered.contains("child-secret"));
		assert!(rendered.contains("https://alice@example.invalid/team/root"));
		assert!(rendered.contains("https://alice@example.invalid/team/child"));
	}

	#[test]
	fn fetch_source_identity_uses_the_module_config_and_worktree_base() {
		let temporary = tempfile::tempdir().unwrap();
		let worktree = temporary.path().join("consumer/modules/one");
		let config =
			GitConfig::parse("[url \"../../../source\"]\n\tinsteadOf = module-alias:\n").unwrap();
		let resolved = SubmoduleTransfer::resolve_source(&config, &worktree, "module-alias:").unwrap();

		assert_eq!(resolved.rewritten, "../../../source");
		assert_eq!(
			resolved.identity,
			temporary.path().join("source").to_str().unwrap()
		);
	}

	#[tokio::test]
	async fn staged_repository_creation_uses_the_retained_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let original = temporary.path().join("repository");
		let retained = temporary.path().join("retained");
		std::fs::create_dir(&original).unwrap();
		let directory = Dir::open_ambient_dir(&original, ambient_authority()).unwrap();
		std::fs::rename(&original, &retained).unwrap();
		std::fs::create_dir(&original).unwrap();

		let _repository = create_repository::<Sha256>(directory, GitConfig::parse("").unwrap())
			.await
			.unwrap();

		assert!(retained.join("objects/pack").is_dir());
		assert!(!original.join("objects").exists());
	}

	#[tokio::test]
	async fn fetched_repository_writes_use_the_retained_directory() {
		let temporary = tempfile::tempdir().unwrap();
		let original = temporary.path().join("repository");
		let retained = temporary.path().join("retained");
		std::fs::create_dir(&original).unwrap();
		let directory = Dir::open_ambient_dir(&original, ambient_authority()).unwrap();
		std::fs::rename(&original, &retained).unwrap();
		std::fs::create_dir(&original).unwrap();

		let repository = repository_from_dir::<Sha256>(directory, GitConfig::parse("").unwrap());
		repository
			.objects()
			.file_store()
			.write_path_if_absent("FETCH_HEAD", b"retained")
			.await
			.unwrap();

		assert_eq!(
			std::fs::read(retained.join("FETCH_HEAD")).unwrap(),
			b"retained"
		);
		assert!(!original.join("FETCH_HEAD").exists());
	}

	#[tokio::test]
	async fn prepared_local_source_keeps_the_validated_repository_capabilities() {
		let temporary = tempfile::tempdir().unwrap();
		let original = temporary.path().join("source");
		let retained = temporary.path().join("retained");
		std::fs::create_dir(&original).unwrap();
		std::fs::write(
			original.join("config"),
			"[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha256\n",
		)
		.unwrap();
		let common = Dir::open_ambient_dir(&original, ambient_authority()).unwrap();
		let git = Dir::open_ambient_dir(&original, ambient_authority()).unwrap();
		let prepared = PreparedTransport::Local {
			files: Backend::new(common, git),
		};

		std::fs::rename(&original, &retained).unwrap();
		std::fs::create_dir(&original).unwrap();
		std::fs::write(
			original.join("config"),
			"[core]\n\trepositoryformatversion = 0\n",
		)
		.unwrap();

		let PreparedTransport::Local { files } = prepared else {
			unreachable!();
		};
		assert_eq!(detect_hash_kind(&files).await.unwrap(), HashKind::Sha256);
		let source: Repository<_, Sha256> = Repository::new(ObjectStore::new(files.shared_handle()));
		assert_eq!(
			source
				.read_config()
				.await
				.unwrap()
				.get_string("extensions", None, "objectformat"),
			Some("sha256")
		);
		assert!(retained.join("config").is_file());
	}
}
