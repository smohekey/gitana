//! The stash list and git's abbreviated ids, for [`Repository`](crate::Repository)'s stash methods.

use std::collections::{BTreeSet, HashMap};

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::GitPath;

use crate::{Repository, RepositoryError, StashEntry};

/// The stashes, newest first (`stash@{0}`), as `git stash list` lists `refs/stash`'s reflog.
pub(crate) async fn list<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<Vec<StashEntry<H>>, RepositoryError> {
	let entries = repo.refs().reflog("refs/stash").await?;
	Ok(
		entries
			.into_iter()
			.rev()
			.filter_map(|entry| entry.new.map(|commit| (commit, entry)))
			.enumerate()
			.map(|(index, (commit, entry))| StashEntry {
				index,
				commit,
				message: String::from_utf8_lossy(&entry.message).into_owned(),
				committer: String::from_utf8_lossy(&entry.committer).into_owned(),
			})
			.collect(),
	)
}

/// `id` abbreviated as git abbreviates it (`find_unique_abbrev` with `core.abbrev`): `core.abbrev`'s
/// length when it's a number, the whole id for `no`, else git's automatic length (the packed
/// objects' count's bit length, halved and rounded up, at least 7), then longer until no other
/// object shares the prefix.
pub(crate) async fn abbreviate<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	id: ObjectId<H>,
) -> Result<String, RepositoryError> {
	let hex = id.to_hex();
	let configured = repo.effective_config().await?;
	let configured = configured.get_string("core", None, "abbrev");
	let start = match configured.map(str::trim) {
		Some(value)
			if ["no", "false", "off"]
				.iter()
				.any(|no| value.eq_ignore_ascii_case(no)) =>
		{
			return Ok(hex);
		}
		Some(value) if !value.eq_ignore_ascii_case("auto") => match value.parse::<usize>() {
			Ok(length) if (4..=hex.len()).contains(&length) => length,
			_ => automatic_length(repo).await?,
		},
		_ => automatic_length(repo).await?,
	};
	let mut length = start.min(hex.len());
	while length < hex.len()
		&& repo
			.objects()
			.find_by_prefix(&hex[..length])
			.await?
			.iter()
			.any(|other| *other != id)
	{
		length += 1;
	}
	Ok(hex[..length].to_owned())
}

/// git's automatic abbreviation length: the bit length of the packed objects' count, plus one,
/// halved and rounded up, at least 7.
async fn automatic_length<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<usize, RepositoryError> {
	let count = repo.objects().approximate_object_count().await?;
	let bits = (usize::BITS - count.leading_zeros()) as usize;
	Ok(bits.div_ceil(2).max(7))
}

/// The files `stash@{index}` holds, path-sorted, as `git stash show --name-only` lists them: those
/// its working tree (`w`) changes from its base, and with `include_untracked` its untracked ones
/// (`u`). `None` when there's no such stash. (Renames aren't detected: a renamed file is listed
/// by both names.)
pub(crate) async fn files<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	index: usize,
	include_untracked: bool,
) -> Result<Option<Vec<GitPath>>, RepositoryError> {
	let Some(entry) = list(repo).await?.into_iter().nth(index) else {
		return Ok(None);
	};
	let commit = crate::revision::read_commit(repo, entry.commit).await?;
	let Some(base) = commit.parents.first() else {
		return Err(RepositoryError::InvalidRef(format!(
			"{} is not a stash-like commit",
			entry.commit
		)));
	};
	let flat =
		|entries: Vec<(GitPath, String, ObjectId<H>)>| -> HashMap<GitPath, (String, ObjectId<H>)> {
			entries
				.into_iter()
				.map(|(path, mode, oid)| (path, (mode, oid)))
				.collect()
		};
	let base = flat(repo.read_tree(repo.commit_tree(*base).await?).await?);
	let work = flat(repo.read_tree(commit.tree).await?);
	let mut files: BTreeSet<GitPath> = base
		.keys()
		.chain(work.keys())
		.filter(|path| base.get(*path) != work.get(*path))
		.cloned()
		.collect();
	if include_untracked && let Some(untracked) = commit.parents.get(2) {
		let tree = repo.commit_tree(*untracked).await?;
		files.extend(
			repo
				.read_tree(tree)
				.await?
				.into_iter()
				.map(|(path, _, _)| path),
		);
	}
	Ok(Some(files.into_iter().collect()))
}
