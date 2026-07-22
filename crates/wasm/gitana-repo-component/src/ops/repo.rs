//! Repository-level operations: init layout, config.

use std::path::Path;

use gitana_config::{ConfigError, GitConfig, GitConfigSource, IncludeContext};
use gitana_file_store::{FileStore, FileStoreError};
use gitana_file_store_local::{LocalFileStore, WorktreeFileStore};
use gitana_object::HashAlgorithm;
use gitana_repository::{Repository, RepositoryError};

use crate::bindings::exports::gitana::repo::porcelain::{
	RepackReport as WitRepackReport, RepoError,
};

use super::FileStoreIncludeResolver;
use super::repo_error;

/// git's empty directory skeleton, created by `init` so the repository is
/// recognizable to stock git tooling (the file store itself never creates
/// value-less directories).
const SKELETON: [&str; 5] = [
	"info",
	"objects/info",
	"objects/pack",
	"refs/heads",
	"refs/tags",
];

pub(crate) async fn init_layout(store: &LocalFileStore) -> Result<(), RepoError> {
	for dir in SKELETON {
		store
			.create_dir_all(dir)
			.await
			.map_err(|error| repo_error(RepositoryError::FileStore(error)))?;
	}
	Ok(())
}

/// Write the fresh-repo metadata (idempotent — `write_path_if_absent` under the
/// hood) and validate the resulting config matches the requested hash algorithm:
/// re-initializing a repository of another format fails with `unsupported-format`.
pub(crate) async fn init_repo<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
) -> Result<(), RepoError> {
	repo.init().await.map_err(repo_error)?;
	repo.open().await.map_err(repo_error)?;
	Ok(())
}

/// git's geometric factor, as used by `gta repack --geometric` / `gta gc`.
const GEOMETRIC_FACTOR: u64 = 2;

pub(crate) async fn repack<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
	geometric: bool,
) -> Result<Option<WitRepackReport>, RepoError> {
	let max_pack_size = repo.pack_size_limit().await.map_err(repo_error)?;
	let report = if geometric {
		repo
			.objects()
			.repack_geometric(max_pack_size, GEOMETRIC_FACTOR)
			.await
	} else {
		repo.objects().repack(max_pack_size).await
	}
	.map_err(|error| repo_error(RepositoryError::ObjectStore(error)))?;
	Ok(report.map(|report| WitRepackReport {
		packed_objects: report.packed_objects as u64,
		packs_written: report.packs_written as u64,
		packs_kept: report.packs_kept as u64,
		packs_removed: report.packs_removed as u64,
		loose_removed: report.loose_removed as u64,
	}))
}

pub(crate) async fn read_config<H: HashAlgorithm>(
	repo: &Repository<WorktreeFileStore, H>,
) -> Result<String, RepoError> {
	let bytes = repo
		.objects()
		.file_store()
		.read_path("config")
		.await
		.map_err(|error| repo_error(RepositoryError::FileStore(error)))?;
	String::from_utf8(bytes).map_err(|_| RepoError::Invalid("config is not UTF-8".to_owned()))
}

/// Expand the repository's local `config` — git's `[include]`/`includeIf` directives — and install the
/// result as the repository's effective configuration, so **every** in-component consumer honours
/// included values: `pack.packSizeLimit` (`repack`), `remote.origin.fetch`/`tagOpt` (`fetch`, via
/// `gitana_porcelain`), and `core.logAllRefUpdates` (ref writes). `read-config` stays the raw file (git's
/// plumbing contract; the host parses it).
///
/// Config is read **once at open** (like a git process reading its config at startup), a snapshot — a
/// handle does not observe a `config`/HEAD change made after it opened; reopen to pick one up. The
/// component's config layers are the common `config` and, under `extensions.worktreeConfig`, the
/// per-worktree `config.worktree` above it (no global/system/`-c`), and its FileStore capability is
/// path-less, so — matching git as far as the capability allows — `onbranch:` (via HEAD) and
/// `hasconfig:remote.*.url:` (via a local remote-URL pre-scan) apply, as do relative includes resolved
/// under the git dir, while `gitdir:` conditions never match (no gitdir path in a descriptor) and an
/// include outside the capability is handled by
/// [`FileStoreIncludeResolver`](super::FileStoreIncludeResolver). A malformed config, an include
/// cycle/over-depth, git's `hasconfig` paradox, a directory include target, or a `~`-with-no-home aborts
/// the open, as git aborts on a bad config.
pub(crate) async fn install_effective_config<H: HashAlgorithm>(
	repo: &mut Repository<WorktreeFileStore, H>,
) -> Result<(), RepoError> {
	if let Some(config) = expand_local_config(repo.objects().file_store()).await? {
		repo.set_effective_config(config);
	}
	Ok(())
}

/// Read and include-expand the repository config, returning the effective [`GitConfig`], or `None` when
/// even the common `config` is absent. See [`install_effective_config`] for the capability semantics.
///
/// git's `config` lives in the *common* dir; when `extensions.worktreeConfig` is enabled (read from the
/// **unexpanded** common config, git's rule), `<git-dir>/config.worktree` is layered **above** it —
/// `local < config.worktree`. Each layer's relative `[include]` targets resolve against its own store
/// (common vs per-worktree), routing an include named like a per-worktree file (`config.worktree`,
/// `HEAD`, …) to the right directory rather than through the per-path routing. The whole-config
/// `hasconfig` pre-scan spans both layers.
async fn expand_local_config(store: &WorktreeFileStore) -> Result<Option<GitConfig>, RepoError> {
	let common = store.common();
	let Some(common_source) = parse_config(common, "config").await? else {
		return Ok(None);
	};

	// `extensions.worktreeConfig` is a repository-format extension git honours only from the repo-local
	// config, read directly (before includes). A bad boolean aborts, as git does.
	let worktree_config = common_source
		.get_bool_validated("extensions", None, "worktreeconfig")
		.map_err(config_error)?
		.unwrap_or(false);
	let worktree_source = if worktree_config {
		parse_config(store.worktree(), "config.worktree").await?
	} else {
		None
	};

	// Ordered lowest → highest precedence, each paired with the store its includes resolve against.
	let mut layers: Vec<(GitConfigSource, &LocalFileStore)> = vec![(common_source, common)];
	if let Some(worktree_source) = worktree_source {
		layers.push((worktree_source, store.worktree()));
	}

	let branch = head_branch(store).await;
	// Each config file is its store root, so its directory is empty: a relative include resolves to a
	// plain store-relative path. The lexical and real dirs coincide (no symlinks in the store).
	let dir = Path::new("");

	// git's whole-config remote-URL pre-scan spans every layer (supplying `hasconfig` URLs and firing the
	// paradox guard) before any expansion.
	let prescan_ctx = include_context(branch.as_deref(), None);
	let mut urls: Vec<String> = Vec::new();
	let mut has_hasconfig = false;
	let mut forbidden_url = false;
	for (source, layer_store) in &layers {
		let resolver = FileStoreIncludeResolver::new(*layer_store);
		let scan = source
			.scan_remote_urls(dir, dir, &prescan_ctx, &resolver)
			.await
			.map_err(config_error)?;
		urls.extend(scan.urls);
		has_hasconfig |= scan.has_hasconfig;
		forbidden_url |= scan.forbidden_url;
	}
	if has_hasconfig && forbidden_url {
		return Err(config_error(ConfigError::HasconfigIncludeSetsRemoteUrl));
	}

	let url_refs: Vec<&str> = urls.iter().map(String::as_str).collect();
	let ctx = include_context(branch.as_deref(), Some(url_refs.as_slice()));
	for (source, layer_store) in &mut layers {
		let resolver = FileStoreIncludeResolver::new(*layer_store);
		source
			.expand_includes(dir, dir, &ctx, &resolver)
			.await
			.map_err(config_error)?;
	}

	Ok(Some(GitConfig::from_sources(
		layers.into_iter().map(|(source, _)| source).collect(),
	)))
}

/// Read and parse a config file through `store`, or `None` when it is absent (git skips an absent
/// layer). A present-but-unreadable/unparseable file aborts, as git aborts on a bad config.
async fn parse_config(
	store: &LocalFileStore,
	path: &str,
) -> Result<Option<GitConfigSource>, RepoError> {
	match store.read_path(path).await {
		Ok(bytes) => {
			let text =
				String::from_utf8(bytes).map_err(|_| RepoError::Invalid(format!("{path} is not UTF-8")))?;
			Ok(Some(GitConfigSource::parse(&text).map_err(config_error)?))
		}
		Err(FileStoreError::NotFound) => Ok(None),
		Err(error) => Err(repo_error(RepositoryError::FileStore(error))),
	}
}

/// The include context the component drives expansion with: `onbranch:` from HEAD; no `$HOME`, gitdir
/// path, or `$PWD` (so `gitdir:` never matches and there is no `gitdir_absolute` candidate).
fn include_context<'a>(
	branch: Option<&'a str>,
	remote_urls: Option<&'a [&'a str]>,
) -> IncludeContext<'a> {
	IncludeContext {
		home: None,
		gitdir: None,
		gitdir_absolute: None,
		branch,
		remote_urls,
	}
}

/// The short branch name of a symbolic `HEAD` (`ref: refs/heads/<name>`) for `onbranch:` matching, read
/// through the store. A detached HEAD (a raw object id) or an absent/unreadable HEAD yields `None`, so
/// every `onbranch:` condition is then non-matching, as in git.
async fn head_branch(store: &WorktreeFileStore) -> Option<String> {
	let bytes = store.read_path("HEAD").await.ok()?;
	let text = String::from_utf8(bytes).ok()?;
	let target = text.trim_end().strip_prefix("ref:")?.trim();
	target.strip_prefix("refs/heads/").map(str::to_owned)
}

/// Map a config parse/expansion error to the WIT [`RepoError`], so a bad config aborts the open.
fn config_error(error: ConfigError) -> RepoError {
	RepoError::Invalid(error.to_string())
}
