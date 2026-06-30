//! Runtime → compile-time hash-algorithm dispatch.
//!
//! A repository's object hash is a runtime fact (read from `.git/config` or negotiated
//! over the wire), but the engine is generic over a compile-time `H`. This module is the
//! single bridge: [`detect_algorithm`] reads the runtime [`HashKind`], and the
//! [`on_repo`]/[`on_worktree`] dispatchers pick the matching `H` and hand a concrete
//! `Repository<_, H>` / `WorkTree<_, H>` to a command whose body is written once, generic
//! over `H`. Adding a third algorithm later touches only this file.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use gitana_file_store_local::LocalFileStore;
use gitana_object::{HashAlgorithm, ObjectId, Sha1, Sha256};
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

use crate::repo;

/// Runtime tag for a repository's object hash — the value-level counterpart to the
/// type-level [`Sha1`]/[`Sha256`] markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashKind {
	Sha1,
	Sha256,
}

impl HashKind {
	/// The git `extensions.objectformat` name (`"sha1"` / `"sha256"`).
	pub fn name(self) -> &'static str {
		match self {
			HashKind::Sha1 => "sha1",
			HashKind::Sha256 => "sha256",
		}
	}
}

/// Read a repository's object-hash algorithm from `<git_dir>/config` without committing to
/// a type. git treats an absent `extensions.objectformat` as sha1.
pub fn detect_algorithm(git_dir: &Path) -> Result<HashKind> {
	let text = std::fs::read_to_string(git_dir.join("config"))
		.map_err(|error| anyhow!("reading {}/config: {error}", git_dir.display()))?;
	let config = gitana_config::GitConfig::parse(&text).map_err(|error| anyhow!("{error}"))?;
	match config
		.get_string("extensions", None, "objectformat")
		.unwrap_or("sha1")
	{
		"sha256" => Ok(HashKind::Sha256),
		"sha1" => Ok(HashKind::Sha1),
		other => bail!("unsupported object format: {other}"),
	}
}

/// A command that needs only the object graph and refs, written once over the repo's hash
/// algorithm `H`.
pub trait RepoCommand {
	async fn run<H: HashAlgorithm>(self, repo: Repository<LocalFileStore, H>) -> Result<()>;
}

/// A command that needs the working tree (index + work dir), also given the pathspec
/// `prefix` (the `/`-joined work-tree-relative subdirectory the command was invoked from).
pub trait WorkTreeCommand {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<LocalFileStore, H>,
		prefix: String,
	) -> Result<()>;
}

/// Discover the repository containing `cwd`, then run `command` under the repo's hash
/// algorithm. The single runtime→type bridge for object-graph commands.
pub async fn on_repo<C: RepoCommand>(cwd: &Path, command: C) -> Result<()> {
	let (_work, git) = repo::discover(cwd)?;
	match detect_algorithm(&git)? {
		HashKind::Sha1 => command.run(repo::open_generic::<Sha1>(&git)).await,
		HashKind::Sha256 => command.run(repo::open_generic::<Sha256>(&git)).await,
	}
}

/// Discover the working tree containing `cwd`, then run `command` under the repo's hash
/// algorithm. Errors in a bare repository (no work tree).
pub async fn on_worktree<C: WorkTreeCommand>(cwd: &Path, command: C) -> Result<()> {
	let (work, git, prefix) = repo::discover_worktree_with_prefix(cwd)?;
	match detect_algorithm(&git)? {
		HashKind::Sha1 => {
			let wt = WorkTree::new(repo::open_generic::<Sha1>(&git), work, git);
			command.run(wt, prefix).await
		}
		HashKind::Sha256 => {
			let wt = WorkTree::new(repo::open_generic::<Sha256>(&git), work, git);
			command.run(wt, prefix).await
		}
	}
}

/// A command that operates on a single object named by a revision `spec`, written once
/// over the repo's hash algorithm. The dispatcher resolves the spec (including the
/// index-relative `:<path>` forms, which need the work tree) before handing over the id.
pub trait ObjectCommand {
	async fn run<H: HashAlgorithm>(
		self,
		repo: Repository<LocalFileStore, H>,
		oid: ObjectId<H>,
	) -> Result<()>;
}

/// Resolve `spec` to an object in the repository containing `cwd`, then run `command`
/// under the repo's hash algorithm.
pub async fn on_object<C: ObjectCommand>(cwd: &Path, spec: &str, command: C) -> Result<()> {
	let (work, git) = repo::discover(cwd)?;
	match detect_algorithm(&git)? {
		HashKind::Sha1 => {
			let (repo, oid) = resolve_object::<Sha1>(&work, &git, spec).await?;
			command.run(repo, oid).await
		}
		HashKind::Sha256 => {
			let (repo, oid) = resolve_object::<Sha256>(&work, &git, spec).await?;
			command.run(repo, oid).await
		}
	}
}

/// Resolve `spec` to `(repository, oid)` under `H`. An index-relative spec (`:<path>`)
/// opens the work tree (which holds the index); every other spec resolves against the
/// repository alone, so object-only lookups do not require a work tree.
async fn resolve_object<H: HashAlgorithm>(
	work: &Option<PathBuf>,
	git: &Path,
	spec: &str,
) -> Result<(Repository<LocalFileStore, H>, ObjectId<H>)> {
	let repo = repo::open_generic::<H>(git);
	let oid = if spec.starts_with(':') {
		let work = work
			.clone()
			.ok_or_else(|| anyhow!("this operation must be run in a work tree"))?;
		WorkTree::new(repo::open_generic::<H>(git), work, git.to_path_buf())
			.rev_parse(spec)
			.await?
	} else {
		repo.rev_parse(spec).await?
	};
	Ok((repo, oid))
}
