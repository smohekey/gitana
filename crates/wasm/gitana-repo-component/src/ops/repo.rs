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
/// component's only config layer is the local file (no global/system/`-c`), and its FileStore capability
/// is path-less, so — matching git as far as the capability allows — `onbranch:` (via HEAD) and
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

/// Read and include-expand the local `config`, returning the effective [`GitConfig`], or `None` when the
/// file is absent. See [`install_effective_config`] for the capability semantics.
async fn expand_local_config(store: &WorktreeFileStore) -> Result<Option<GitConfig>, RepoError> {
	// git's `config` and its relative `[include]` targets live in the *common* dir, so read them from the
	// common store directly — routing an include named like a per-worktree file (`config.worktree`, …)
	// through the store would send it to the wrong directory. HEAD (for `onbranch:`) stays per-worktree.
	let common = store.common();
	let text = match common.read_path("config").await {
		Ok(bytes) => {
			String::from_utf8(bytes).map_err(|_| RepoError::Invalid("config is not UTF-8".to_owned()))?
		}
		Err(FileStoreError::NotFound) => return Ok(None),
		Err(error) => return Err(repo_error(RepositoryError::FileStore(error))),
	};
	let mut source = GitConfigSource::parse(&text).map_err(config_error)?;
	let branch = head_branch(store).await;
	let resolver = FileStoreIncludeResolver::new(common);
	// The config file is the store root, so its directory is empty: a relative include resolves to a
	// plain store-relative path. The lexical and real dirs coincide (no symlinks in the store).
	let dir = Path::new("");
	// git's whole-config remote-URL pre-scan precedes expansion (it supplies `hasconfig` URLs and fires
	// the paradox guard); here it runs over the single local layer.
	let prescan_ctx = IncludeContext {
		home: None,
		gitdir: None,
		gitdir_absolute: None,
		branch: branch.as_deref(),
		remote_urls: None,
	};
	let scan = source
		.scan_remote_urls(dir, dir, &prescan_ctx, &resolver)
		.await
		.map_err(config_error)?;
	if scan.has_hasconfig && scan.forbidden_url {
		return Err(config_error(ConfigError::HasconfigIncludeSetsRemoteUrl));
	}
	let urls: Vec<&str> = scan.urls.iter().map(String::as_str).collect();
	let ctx = IncludeContext {
		home: None,
		gitdir: None,
		gitdir_absolute: None,
		branch: branch.as_deref(),
		remote_urls: Some(urls.as_slice()),
	};
	source
		.expand_includes(dir, dir, &ctx, &resolver)
		.await
		.map_err(config_error)?;
	Ok(Some(GitConfig::from_sources(vec![source])))
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
