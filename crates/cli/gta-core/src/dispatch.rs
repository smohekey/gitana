//! Runtime → compile-time hash-algorithm dispatch.
//!
//! A repository's object hash is a runtime fact (read from `.git/config` or negotiated
//! over the wire), but the engine is generic over a compile-time `H`. This module is the
//! single bridge: [`detect_algorithm`] reads the runtime [`HashKind`], and the
//! [`on_repo`]/[`on_worktree`] dispatchers pick the matching `H` and hand a concrete
//! `Repository<_, H>` / `WorkTree<_, H>` to a command whose body is written once, generic
//! over `H`. Adding a third algorithm later touches only this file.

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

use crate::Backend;
use crate::repo::{self, Discovered};

/// Read a repository's object-hash algorithm from `<common_dir>/config` without committing to a
/// type. `common_dir` is the repository's shared git directory (the same as the git directory for
/// an ordinary repo, the main `.git` for a linked worktree — `config` is always shared). git treats
/// an absent `extensions.objectformat` as sha1.
pub fn detect_algorithm(common_dir: &Path) -> Result<HashKind> {
	let text = std::fs::read_to_string(common_dir.join("config"))
		.map_err(|error| anyhow!("reading {}/config: {error}", common_dir.display()))?;
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
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()>;
}

/// A command that needs the working tree (index + work dir), also given the pathspec
/// `prefix` (the `/`-joined work-tree-relative subdirectory the command was invoked from).
pub trait WorkTreeCommand {
	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, H>,
		prefix: String,
	) -> Result<()>;
}

/// Discover the repository containing `cwd`, then run `command` under the repo's hash
/// algorithm. The single runtime→type bridge for object-graph commands.
pub async fn on_repo<C: RepoCommand>(cwd: &Path, command: C) -> Result<()> {
	let found = repo::discover(cwd)?;
	match detect_algorithm(&found.common_dir)? {
		HashKind::Sha1 => command.run(open::<Sha1>(&found)).await,
		HashKind::Sha256 => command.run(open::<Sha256>(&found)).await,
	}
}

/// Discover the working tree containing `cwd`, then run `command` under the repo's hash
/// algorithm. Errors in a bare repository (no work tree).
pub async fn on_worktree<C: WorkTreeCommand>(cwd: &Path, command: C) -> Result<()> {
	let (found, prefix) = repo::discover_worktree_with_prefix(cwd)?;
	let work = found.work.clone().expect("discovered work tree");
	match detect_algorithm(&found.common_dir)? {
		HashKind::Sha1 => {
			let wt = WorkTree::new(open::<Sha1>(&found), work, found.git_dir);
			command.run(wt, prefix).await
		}
		HashKind::Sha256 => {
			let wt = WorkTree::new(open::<Sha256>(&found), work, found.git_dir);
			command.run(wt, prefix).await
		}
	}
}

/// Open the discovered repository under `H`, routing per-worktree and shared files correctly.
fn open<H: HashAlgorithm>(found: &Discovered) -> Repository<Backend, H> {
	repo::open_generic::<H>(&found.git_dir, &found.common_dir)
}

/// A command that operates on a single object named by a revision `spec`, written once
/// over the repo's hash algorithm. The dispatcher resolves the spec (including the
/// index-relative `:<path>` forms, which need the work tree) before handing over the id.
pub trait ObjectCommand {
	async fn run<H: HashAlgorithm>(
		self,
		repo: Repository<Backend, H>,
		oid: ObjectId<H>,
	) -> Result<()>;
}

/// Resolve `spec` to an object in the repository containing `cwd`, then run `command`
/// under the repo's hash algorithm.
pub async fn on_object<C: ObjectCommand>(cwd: &Path, spec: &str, command: C) -> Result<()> {
	let found = repo::discover(cwd)?;
	match detect_algorithm(&found.common_dir)? {
		HashKind::Sha1 => {
			let (repo, oid) = resolve_object::<Sha1>(&found, spec).await?;
			command.run(repo, oid).await
		}
		HashKind::Sha256 => {
			let (repo, oid) = resolve_object::<Sha256>(&found, spec).await?;
			command.run(repo, oid).await
		}
	}
}

/// Resolve `spec` to `(repository, oid)` under `H`. An index-relative spec (`:<path>`)
/// opens the work tree (which holds the index); every other spec resolves against the
/// repository alone, so object-only lookups do not require a work tree.
async fn resolve_object<H: HashAlgorithm>(
	found: &Discovered,
	spec: &str,
) -> Result<(Repository<Backend, H>, ObjectId<H>)> {
	let repo = open::<H>(found);
	let oid = if spec.starts_with(':') {
		let work = found
			.work
			.clone()
			.ok_or_else(|| anyhow!("this operation must be run in a work tree"))?;
		WorkTree::new(open::<H>(found), work, found.git_dir.clone())
			.rev_parse(spec)
			.await?
	} else {
		repo.rev_parse(spec).await?
	};
	Ok((repo, oid))
}
