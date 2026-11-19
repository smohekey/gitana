//! git's stash: putting the working tree's changes away as stash commits (`w`, with the index's
//! `i` and, for untracked files, `u`), and bringing them back.

use std::collections::{HashMap, HashSet};

use gitana_file_store::FileStore;
use gitana_file_store_local::{WorkDirFs, WorkEntryIdentity};
use gitana_identity::Identity;
use gitana_object::{Commit, HashAlgorithm, ObjectId, ObjectKind, parse_commit};
use gitana_path::{GitPath, GitPathspec};
use gitana_repository::{HeadState, Repository, RepositoryError, StashEntry};

use crate::index_lock::IndexLock;
use crate::pathspec::PathspecSet;
use crate::{
	CheckoutMode, ConflictOverwriteError, Converter, Index, IndexEntry, LineEndingChange,
	SparseCheckout, StashApplied, StashApply, StashApplyError, StashPopError, StashPush,
	StashPushError, StashPushed, Stat, WorkTree, WorktreeError,
};

/// The name and email git's stash records for an identity that isn't set (`prepare_fallback_ident`).
const FALLBACK_NAME: &str = "git stash";
const FALLBACK_EMAIL: &str = "git@stash";

/// What [`save`] made, for the store and the clean-up after it.
struct Saved<H: HashAlgorithm> {
	/// The stash commit (`w`), stored once the clean-up's checks pass, with its reflog line's
	/// committer and message.
	commit: ObjectId<H>,
	committer: String,
	message: String,
	line_ending_warnings: Vec<(GitPath, LineEndingChange)>,
	/// The untracked files put away (`u`).
	untracked: Vec<GitPath>,
	skipped_repositories: Vec<GitPath>,
	sparse_pathspecs: Vec<GitPathspec>,
	/// The tracked paths whose index or working file differed from `HEAD`.
	changed: Vec<GitPath>,
	/// With pathspecs, the paths the clean-up leaves as they are, the stash holding no change for
	/// them; their entries take the file's stat, as git's clean-up records it.
	left_alone: Vec<GitPath>,
	head_tree: ObjectId<H>,
	/// Every folder an index entry lies under, which the clean-up keeps.
	tracked_folders: HashSet<GitPath>,
}

/// [`WorkTree::stash_push`].
pub(crate) async fn push<F, W, H>(
	wt: &WorkTree<F, W, H>,
	request: &StashPush,
	identity: &impl Identity,
) -> Result<Option<StashPushed<H>>, StashPushError<H>>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	let Some(head) = repo.refs().resolve_head().await? else {
		return Err(StashPushError::NoInitialCommit);
	};
	let set = PathspecSet::parse(&request.pathspecs, request.prefix.as_bytes())?;
	// One index lock from the save through the clean-up, so nothing staged meanwhile can be lost to
	// it: the stash's commits made, every check the clean-up makes, then the stash stored, the working
	// tree put back (git's `clean` and `reset --hard`, or for pathspecs only the matched paths restored
	// from `HEAD`) and the index written once. An interruption before the store leaves nothing changed;
	// after it, the changes are safe in the stash (`index.lock` kept once the working tree changes).
	let lock = wt.lock_index().await?;
	let index_time = wt.index_time().await?;
	let mut index = wt.load_index().await?;
	let saved = match save(wt, request, &set, head, identity, index_time, &index).await {
		Ok(Some(saved)) => saved,
		Ok(None) => {
			wt.release_index_lock(lock).await;
			return Ok(None);
		}
		Err(error) => {
			wt.release_index_lock(lock).await;
			return Err(error);
		}
	};
	let whole = request.pathspecs.is_empty();
	// The clean-up puts back `HEAD`'s version of each changed path, except, for pathspecs, those
	// outside the sparse checkout, which stay as git's `add` leaves them (its sparse advisory names
	// the pathspecs); a whole push's checkout skips skip-worktree entries itself.
	let sparse = wt.sparse_checkout().await?;
	let cleaned: Vec<&GitPath> = saved
		.changed
		.iter()
		.filter(|path| {
			!index.entry(path).is_some_and(|entry| {
				if whole {
					// As the checkout leaves a path: by the sparse patterns when there are any (a
					// stray skip-worktree bit on an included path is rewritten), else by the bit.
					match sparse.as_ref() {
						Some(matcher) => !matcher.includes(&entry.path),
						None => entry.skip_worktree,
					}
				} else {
					outside_sparse(entry, sparse.as_ref())
				}
			})
		})
		.collect();
	// An untracked file the stash doesn't hold, in the clean-up's way, is refused before anything is
	// stored, rather than lost to it (git's whole push deletes it, its path-limited one fails part-way).
	if let Err(error) = refuse_unsaved(wt, &cleaned, &saved, &index).await {
		wt.release_index_lock(lock).await;
		return Err(error.into());
	}
	if whole && let Err(error) = refuse_submodule_checkouts(wt, &saved, &index, sparse.as_ref()).await
	{
		wt.release_index_lock(lock).await;
		return Err(error.into());
	}
	let reset_committer = match identity.committer_or_default().await {
		Ok(committer) => committer,
		Err(error) => {
			wt.release_index_lock(lock).await;
			return Err(StashPushError::Identity(error));
		}
	};
	let mut stored = None;
	let mut held_head = None;
	let mut not_removed = Vec::new();
	let ready = async {
		// `HEAD` checked, the stash stored, then `HEAD` held to the end, so nothing moves it while the
		// working tree goes back to it (its locks share `packed-refs` with the store, so the store
		// comes first). Moved before the store: refused, nothing stored; in the instant after it:
		// refused with the stash stored and nothing else changed. For the whole tree the hold then
		// takes the rest of git's `reset --hard` (its reflog line and `ORIG_HEAD`, an in-progress
		// operation abandoned, a merge's autostash saved on top of this stash, as git leaves it); for
		// pathspecs it's released unchanged.
		if repo.refs().resolve_head().await? != Some(head) {
			return Err(
				RepositoryError::RefMoved {
					name: "HEAD".to_owned(),
				}
				.into(),
			);
		}
		repo
			.store_stash(saved.commit, &saved.committer, &saved.message)
			.await?;
		stored = Some(stored_entry(repo, saved.commit).await?);
		held_head = Some(
			repo
				.prepare_reset_to_head(head, &reset_committer, "reset: moving to HEAD")
				.await?,
		);
		lock.mark_mutation_started();
		if request.include_untracked {
			// As git: every untracked file removed that can be, and then, if any couldn't, the
			// tracked files left as they are.
			not_removed = clean(
				wt,
				&saved.untracked,
				&saved.tracked_folders,
				(!whole).then_some(&set),
			)?;
			if let Some((path, error)) = not_removed.first() {
				return Err(WorktreeError::Io(std::io::Error::new(
					error.kind(),
					format!("could not remove '{path}': {error}"),
				)));
			}
		}
		Ok(())
	};
	let restored: Vec<GitPathspec> = cleaned
		.iter()
		.map(|path| {
			let mut spec = b":(top,literal)".to_vec();
			spec.extend_from_slice(path.as_bytes());
			GitPathspec::from_bytes(spec)
		})
		.collect::<Result<_, _>>()
		.map_err(|error| WorktreeError::InvalidPath(error.to_string()))?;
	let result = if whole {
		crate::checkout::run_locked(
			wt,
			&lock,
			index_time,
			&mut index,
			saved.head_tree,
			CheckoutMode::Reset,
			request.excludes_file.as_deref(),
			ready,
		)
		.await
	} else if restored.is_empty() {
		ready.await
	} else {
		crate::restore::run_locked(
			wt,
			&lock,
			index_time,
			&mut index,
			Some(saved.head_tree),
			&restored,
			&GitPath::root(),
			ready,
		)
		.await
	};
	let result = result.and_then(|()| {
		for path in &saved.left_alone {
			if let Some(meta) = wt.work().lstat(path)?
				&& let Some(entry) = index
					.entries
					.iter_mut()
					.find(|entry| entry.stage == 0 && entry.path == *path)
			{
				entry.stat = crate::fsmeta::stat_of(&meta);
			}
		}
		Ok(())
	});
	match result {
		Ok(()) => wt.commit_index(lock, &index).await?,
		Err(error) => {
			wt.release_index_lock(lock).await;
			if !not_removed.is_empty()
				&& let Some(entry) = stored
			{
				return Err(StashPushError::Clean {
					pushed: Box::new(StashPushed {
						entry,
						line_ending_warnings: saved.line_ending_warnings,
						skipped_repositories: saved.skipped_repositories,
						sparse_pathspecs: saved.sparse_pathspecs,
					}),
					not_removed,
				});
			}
			return Err(error.into());
		}
	}
	let Some(mut entry) = stored else {
		unreachable!("the clean-up succeeded, so the stash was stored first");
	};
	let Some(held_head) = held_head else {
		unreachable!("the clean-up succeeded, so HEAD was held first");
	};
	if whole {
		held_head
			.finish()
			.await
			.map_err(|error| StashPushError::Reset(error.into()))?;
		// Finishing the reset may have saved a merge's autostash on top of this stash, as git
		// leaves it, so its place is read again.
		if let Ok(listed) = repo.stash_list().await
			&& let Some(moved) = listed
				.into_iter()
				.find(|listed| listed.commit == entry.commit)
		{
			entry = moved;
		}
	} else {
		drop(held_head);
	}
	Ok(Some(StashPushed {
		entry,
		line_ending_warnings: saved.line_ending_warnings,
		skipped_repositories: saved.skipped_repositories,
		sparse_pathspecs: saved.sparse_pathspecs,
	}))
}

/// Whether `entry` lies outside the sparse checkout, as git's `add` tells it: skip-worktree, or
/// beyond the sparse patterns.
fn outside_sparse<H: HashAlgorithm>(
	entry: &IndexEntry<H>,
	sparse: Option<&SparseCheckout>,
) -> bool {
	entry.skip_worktree || sparse.is_some_and(|matcher| !matcher.includes(&entry.path))
}

/// Refuse a clean-up that would delete an untracked file the stash doesn't hold: one in a folder
/// standing where `HEAD` has a file, or a file standing where `HEAD` has a folder. `cleaned` are the
/// paths the clean-up puts back; held means tracked, or among the untracked files put away.
async fn refuse_unsaved<F, W, H>(
	wt: &WorkTree<F, W, H>,
	cleaned: &[&GitPath],
	saved: &Saved<H>,
	index: &Index<H>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let head = wt.repository().read_tree(saved.head_tree).await?;
	let head_files: HashSet<&GitPath> = head
		.iter()
		.filter(|(_, mode, _)| mode != "160000")
		.map(|(path, _, _)| path)
		.collect();
	let tracked: HashSet<&GitPath> = index.entries.iter().map(|entry| &entry.path).collect();
	let put_away: HashSet<&GitPath> = saved.untracked.iter().collect();
	let unsaved = |path: &GitPath| !tracked.contains(path) && !put_away.contains(path);
	for path in cleaned {
		if !head_files.contains(*path) {
			continue;
		}
		// A file where one of the path's folders goes.
		for folder in path.strict_ancestors() {
			match wt.work().lstat(&folder)? {
				Some(meta) if meta.kind.is_dir() => {}
				Some(_) if unsaved(&folder) => return Err(WorktreeError::UntrackedOverwrite(folder)),
				_ => break,
			}
		}
		// A folder where the file goes: everything in it held.
		if wt
			.work()
			.lstat(path)?
			.is_some_and(|meta| meta.kind.is_dir())
		{
			let mut folders = vec![(*path).clone()];
			while let Some(folder) = folders.pop() {
				for entry in wt.work().read_dir(&folder)? {
					let child = folder.join(&entry.name);
					if entry.kind.is_dir() {
						folders.push(child);
					} else if unsaved(&child) {
						return Err(WorktreeError::UntrackedOverwrite(child));
					}
				}
			}
		}
	}
	Ok(())
}

/// Refuse a whole push's clean-up that would put `HEAD`'s file where a populated submodule checkout
/// stands, or `HEAD`'s files into it (a submodule staged in the place of a file or a folder of
/// files, its folder holding anything, a checkout's `.git` included): what's in it isn't the
/// stash's to remove or overwrite (git deletes the one, refuses the other), so it's refused before
/// anything is stored. An entry the checkout leaves (outside the sparse checkout) is passed over.
async fn refuse_submodule_checkouts<F, W, H>(
	wt: &WorkTree<F, W, H>,
	saved: &Saved<H>,
	index: &Index<H>,
	sparse: Option<&SparseCheckout>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let head = wt.repository().read_tree(saved.head_tree).await?;
	let head_files: HashSet<&GitPath> = head
		.iter()
		.filter(|(_, mode, _)| mode != "160000")
		.map(|(path, _, _)| path)
		.collect();
	// The folders `HEAD`'s files lie in: a submodule staged in one's place has the checkout write
	// those files into it.
	let head_folders: HashSet<GitPath> = head_files
		.iter()
		.flat_map(|path| path.strict_ancestors())
		.filter(|folder| !folder.is_root())
		.collect();
	for entry in &index.entries {
		let left = match sparse {
			Some(matcher) => !matcher.includes(&entry.path),
			None => entry.skip_worktree,
		};
		if entry.stage == 0
			&& entry.mode == 0o160000
			&& !left
			&& (head_files.contains(&entry.path) || head_folders.contains(&entry.path))
			&& wt
				.work()
				.lstat(&entry.path)?
				.is_some_and(|meta| meta.kind.is_dir())
			&& !wt.work().read_dir(&entry.path)?.is_empty()
		{
			return Err(WorktreeError::UntrackedOverwrite(entry.path.clone()));
		}
	}
	Ok(())
}

/// The stash just stored as `commit`: `stash@{0}`, unless something moved it meanwhile.
async fn stored_entry<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	commit: ObjectId<H>,
) -> Result<StashEntry<H>, RepositoryError> {
	repo
		.stash_list()
		.await?
		.into_iter()
		.next()
		.filter(|entry| entry.commit == commit)
		.ok_or_else(|| RepositoryError::RefMoved {
			name: "refs/stash".to_owned(),
		})
}

/// Make the stash's commits, as git's `create_stash` does, or `None` when there's nothing to put
/// away. Writes only objects; the caller holds the index lock and loaded `index` under it.
async fn save<F, W, H>(
	wt: &WorkTree<F, W, H>,
	request: &StashPush,
	set: &PathspecSet,
	head: ObjectId<H>,
	identity: &impl Identity,
	index_time: Option<std::time::SystemTime>,
	index: &Index<H>,
) -> Result<Option<Saved<H>>, StashPushError<H>>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	let whole = request.pathspecs.is_empty();
	// Without untracked files, each pathspec must name a tracked file, as git requires; git checks
	// that before the index.
	if !whole && !request.include_untracked {
		for entry in &index.entries {
			set.matches(&entry.path);
		}
		if let Some(spec) = set.unmatched() {
			return Err(StashPushError::UnmatchedPathspec(spec.clone()));
		}
	}
	if index.has_conflicts() {
		let mut paths: Vec<GitPath> = index.unmerged_paths().cloned().collect();
		paths.dedup();
		return Err(StashPushError::Unmerged(paths));
	}
	let selected = |path: &GitPath| whole || set.matches(path);
	let head_commit = read_commit(repo, head).await?;
	let head_entries: HashMap<GitPath, (String, ObjectId<H>)> = repo
		.read_tree(head_commit.tree)
		.await?
		.into_iter()
		.map(|(path, mode, oid)| (path, (mode, oid)))
		.collect();
	// What there is to save, as git's stash tells it (submodules aside): the index against `HEAD`,
	// the working tree against the index, and, if asked, untracked files.
	let staged: Vec<&crate::IndexEntry<H>> = index.entries.iter().filter(|e| e.stage == 0).collect();
	let in_index: HashSet<&GitPath> = staged.iter().map(|entry| &entry.path).collect();
	let mut changed: Vec<GitPath> = Vec::new();
	for entry in &staged {
		if entry.mode == 0o160000 || !selected(&entry.path) {
			continue;
		}
		let same = head_entries
			.get(&entry.path)
			.is_some_and(|(mode, oid)| *oid == entry.oid && *mode == format!("{:o}", entry.mode));
		if !same {
			changed.push(entry.path.clone());
		}
	}
	for (path, (mode, _)) in &head_entries {
		if mode != "160000" && !in_index.contains(path) && selected(path) {
			changed.push(path.clone());
		}
	}
	let file_mode = crate::status::worktree_file_mode(wt).await;
	let mut checkin = Converter::check_in(wt, index, index_time).await?;
	// A selected `.gitattributes` gone from the working tree (or now a folder) no longer governs the
	// files put away, and a symlinked one is read as its target's text, as `add` treats them (git's
	// stash stages them before the files they govern). One outside the sparse checkout stays.
	let sparse = wt.sparse_checkout().await?;
	for entry in staged.iter().filter(|entry| {
		!entry.skip_worktree
			&& crate::attributes::attributes_folder(&entry.path).is_some()
			&& selected(&entry.path)
			&& sparse
				.as_ref()
				.is_none_or(|matcher| matcher.includes(&entry.path))
	}) {
		match wt.work().lstat(&entry.path).map_err(WorktreeError::from)? {
			Some(meta) if meta.kind.is_file() => {}
			Some(meta) if meta.kind.is_symlink() => {
				let target = wt
					.work()
					.read_link(&entry.path)
					.map_err(WorktreeError::from)?;
				checkin.stage_link(&entry.path, target);
			}
			_ => checkin.forget_indexed(&entry.path),
		}
	}
	let mut unstaged: Vec<GitPath> = Vec::new();
	let mut present_skipped: Vec<GitPath> = Vec::new();
	for entry in &staged {
		if !selected(&entry.path) {
			continue;
		}
		// A submodule is its own repository's, while it's a folder (or absent); a file or symlink
		// standing in its place is saved. A path outside the sparse checkout is passed over while
		// its file is absent; one that's there again is saved, as git (clearing the bit of a file it
		// finds present) saves it.
		let on_disk = wt.work().lstat(&entry.path).map_err(WorktreeError::from)?;
		if entry.mode == 0o160000 && on_disk.as_ref().is_none_or(|meta| meta.kind.is_dir()) {
			continue;
		}
		if entry.skip_worktree {
			if on_disk.is_none() {
				continue;
			}
			present_skipped.push(entry.path.clone());
		}
		// A file staged as new and since deleted keeps the index's copy in `w`, as git's stash
		// (comparing the working tree with `HEAD`, which lacks it) passes it over.
		if !head_entries.contains_key(&entry.path)
			&& wt
				.work()
				.lstat(&entry.path)
				.map_err(WorktreeError::from)?
				.is_none()
		{
			continue;
		}
		if crate::status::worktree_change(&mut checkin, entry, &entry.path, file_mode).await? != ' ' {
			unstaged.push(entry.path.clone());
		}
	}
	// A `HEAD` path removed from the index alone (`rm --cached`) is saved with its working content
	// too, as git's working-tree commit covers `HEAD`'s paths, since the clean-up puts `HEAD`'s copy
	// back.
	for (path, (mode, _)) in &head_entries {
		if mode != "160000"
			&& !in_index.contains(path)
			&& selected(path)
			&& wt
				.work()
				.lstat(path)
				.map_err(WorktreeError::from)?
				.is_some_and(|meta| !meta.kind.is_dir())
		{
			unstaged.push(path.clone());
		}
	}
	let fold = crate::excludes::ignore_case(wt).await?;
	// Untracked files, and the nested repositories git lists among them but leaves out.
	let mut untracked: Vec<GitPath> = Vec::new();
	let mut skipped_repositories: Vec<GitPath> = Vec::new();
	if request.include_untracked {
		let others =
			crate::ls_files::others(wt, index, true, fold, request.excludes_file.as_deref()).await?;
		for other in others {
			if other.directory {
				if whole || set.matches_directory(&other.path) {
					skipped_repositories.push(other.path);
				}
			} else if selected(&other.path) {
				untracked.push(other.path);
			}
		}
	}
	if changed.is_empty()
		&& unstaged.is_empty()
		&& untracked.is_empty()
		&& skipped_repositories.is_empty()
	{
		return Ok(None);
	}
	// git can't save an intent-to-add entry, wherever it lies.
	let intent_to_add: Vec<GitPath> = index
		.entries
		.iter()
		.filter(|entry| entry.intent_to_add)
		.map(|entry| entry.path.clone())
		.collect();
	if !intent_to_add.is_empty() {
		return Err(StashPushError::IntentToAdd(intent_to_add));
	}
	// Every staged version put away must be readable, since the clean-up may remove its only other
	// copy: each one differing from `HEAD` is read (and so verified against its id) before anything
	// is written, a missing one refused as git's `write-tree` refuses it ("invalid object"), a corrupt
	// one as well (git saves it and loses the file).
	for path in &changed {
		let Some(entry) = index.entry(path) else {
			continue;
		};
		if entry.mode == 0o160000 {
			continue;
		}
		if !repo
			.objects()
			.exists_object(&entry.oid)
			.await
			.map_err(RepositoryError::from)?
		{
			return Err(RepositoryError::MissingObject(entry.oid.to_hex()).into());
		}
		match repo.objects().read_object(&entry.oid).await {
			Ok((ObjectKind::Blob, _)) => {}
			Ok((kind, _)) => {
				return Err(
					RepositoryError::InvalidRef(format!(
						"{} is a {kind:?}, not a blob, for '{path}'",
						entry.oid.to_hex()
					))
					.into(),
				);
			}
			Err(error) => return Err(RepositoryError::from(error).into()),
		}
	}
	// The pathspecs that reach entries outside the sparse checkout and no others (git's `add`
	// advisory). Outside, as git's `add` tells it: skip-worktree, or beyond the sparse patterns.
	let sparse_pathspecs: Vec<GitPathspec> = if whole {
		Vec::new()
	} else {
		let sparse = wt.sparse_checkout().await?;
		set
			.positives()
			.filter(|(_, pathspec)| {
				let (mut outside, mut inside) = (false, false);
				for entry in staged.iter().filter(|entry| pathspec.matches(&entry.path)) {
					if outside_sparse(entry, sparse.as_ref()) {
						outside = true;
					} else {
						inside = true;
					}
				}
				outside && !inside
			})
			.map(|(spec, _)| spec.clone())
			.collect()
	};

	let branch = match repo.refs().read_head().await? {
		HeadState::Symbolic(name) => name.strip_prefix("refs/heads/").unwrap_or(&name).to_owned(),
		_ => "(no branch)".to_owned(),
	};
	let label = format!(
		"{branch}: {} {}",
		repo.abbreviate(head).await?,
		subject(&head_commit.message)
	);
	let author = identity
		.author_or(FALLBACK_NAME, FALLBACK_EMAIL)
		.await
		.map_err(StashPushError::Identity)?;
	let committer = identity
		.committer_or(FALLBACK_NAME, FALLBACK_EMAIL)
		.await
		.map_err(StashPushError::Identity)?;

	// `i`: the whole index, on `HEAD`.
	let i_tree = repo.write_tree(&index.tree_entries()).await?;
	let i = repo
		.create_commit(
			i_tree,
			vec![head],
			&author,
			&committer,
			&format!("index on {label}\n"),
		)
		.await?;
	// `u`: the untracked files, on nothing; empty when only nested repositories were found, as git's.
	let u = if untracked.is_empty() && skipped_repositories.is_empty() {
		None
	} else {
		let mut u_index = Index::new();
		for path in &untracked {
			wt.stage_file(&mut u_index, path, None, fold, &mut checkin)
				.await?;
		}
		let u_tree = repo.write_tree(&u_index.tree_entries()).await?;
		Some(
			repo
				.create_commit(
					u_tree,
					Vec::new(),
					&author,
					&committer,
					&format!("untracked files on {label}\n"),
				)
				.await?,
		)
	};
	// `w`: `i`'s tree with the selected working files as they'd be staged, on `HEAD`, `i` and `u`.
	let mut w_index = index.clone();
	// Staged as files, not passed over as outside the sparse checkout.
	for entry in w_index.entries.iter_mut() {
		if entry.stage == 0 && present_skipped.contains(&entry.path) {
			entry.skip_worktree = false;
		}
	}
	for path in &unstaged {
		wt.stage_file(&mut w_index, path, None, fold, &mut checkin)
			.await?;
	}
	let w_tree = repo.write_tree(&w_index.tree_entries()).await?;
	let message = match request.message.as_deref() {
		Some(message) if !message.is_empty() => format!("On {branch}: {message}"),
		_ => format!("WIP on {label}"),
	};
	let mut parents = vec![head, i];
	parents.extend(u);
	let w = repo
		.create_commit(w_tree, parents, &author, &committer, &message)
		.await?;

	changed.extend(unstaged);
	changed.sort();
	changed.dedup();
	// A path-limited clean-up leaves a path the stash holds no change for (its index and working
	// versions both `HEAD`'s: a difference git's comparison normalises away), so its bytes stay, as
	// git's clean-up (failing there) leaves them.
	let mut left_alone: Vec<GitPath> = Vec::new();
	if !whole {
		let version =
			|entry: Option<&IndexEntry<H>>| entry.map(|entry| (format!("{:o}", entry.mode), entry.oid));
		let (put_back, unchanged): (Vec<GitPath>, Vec<GitPath>) =
			changed.into_iter().partition(|path| {
				let head = head_entries.get(path).cloned();
				// Only a file (or symlink) whose difference is normalised away: anything else in its
				// place (a folder, say) is a change the stash couldn't hold, which the clean-up meets.
				let a_file = wt
					.work()
					.lstat(path)
					.is_ok_and(|meta| meta.is_some_and(|meta| !meta.kind.is_dir()));
				!a_file || version(index.entry(path)) != head || version(w_index.entry(path)) != head
			});
		changed = put_back;
		left_alone = unchanged;
	}
	let mut tracked_folders = HashSet::new();
	for entry in &index.entries {
		let mut folder = entry.path.parent();
		while let Some(parent) = folder {
			if parent.is_root() || !tracked_folders.insert(parent.clone()) {
				break;
			}
			folder = parent.parent();
		}
	}
	Ok(Some(Saved {
		commit: w,
		committer,
		message,
		line_ending_warnings: checkin.outcome().line_ending_warnings,
		untracked,
		skipped_repositories,
		sparse_pathspecs,
		changed,
		left_alone,
		head_tree: head_commit.tree,
		tracked_folders,
	}))
}

/// The commit `id`.
async fn read_commit<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	id: ObjectId<H>,
) -> Result<Commit<H>, RepositoryError> {
	let (kind, payload) = repo.objects().read_object(&id).await?;
	if kind != ObjectKind::Commit {
		return Err(RepositoryError::InvalidRef(format!("{id} is not a commit")));
	}
	Ok(parse_commit::<H>(&payload)?)
}

/// git's subject of `message` (`%s`): its first paragraph, each line's trailing whitespace trimmed,
/// joined by spaces (leading blank lines skipped).
pub(crate) fn subject(message: &str) -> String {
	let mut lines = Vec::new();
	for line in message.split('\n') {
		let line = line.trim_end_matches([' ', '\t', '\n', '\r']);
		if line.is_empty() {
			if lines.is_empty() {
				continue;
			}
			break;
		}
		lines.push(line);
	}
	lines.join(" ")
}

/// Remove the untracked `files` put away, and the untracked folders left with nothing, as git's stash
/// does with `git clean -d`: within `scope` when given (the push's pathspecs), never a folder an index
/// entry lies under, a nested repository, or `.git`. In git's order too: each folder that goes whole
/// (everything under it put away) and each other file, by name (a folder as `name/`), a folder walked
/// in the order the filesystem lists it. Each file or folder that can't be removed is passed over and
/// returned, with why, as git's `clean` warns and goes on.
fn clean<F, W, H>(
	wt: &WorkTree<F, W, H>,
	files: &[GitPath],
	tracked_folders: &HashSet<GitPath>,
	scope: Option<&PathspecSet>,
) -> Result<Vec<(GitPath, std::io::Error)>, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let saved: HashSet<&GitPath> = files.iter().collect();
	let mut wholes = Vec::new();
	plan_wholes(
		wt,
		&GitPath::root(),
		&saved,
		tracked_folders,
		scope,
		&mut wholes,
	)?;
	let whole_set: HashSet<&GitPath> = wholes.iter().collect();
	// Git's list: the folders that go whole, and the files outside them, by name.
	let mut entries: Vec<(Vec<u8>, &GitPath, bool)> = wholes
		.iter()
		.map(|folder| {
			let mut key = folder.as_bytes().to_vec();
			key.push(b'/');
			(key, folder, true)
		})
		.collect();
	entries.extend(
		files
			.iter()
			.filter(|path| {
				!path
					.strict_ancestors()
					.iter()
					.any(|folder| whole_set.contains(folder))
			})
			.map(|path| (path.as_bytes().to_vec(), path, false)),
	);
	entries.sort_by(|a, b| a.0.cmp(&b.0));
	let mut not_removed = Vec::new();
	for (_, path, folder) in entries {
		if folder {
			remove_whole(wt, path, &saved, &mut not_removed)?;
			continue;
		}
		crate::checkout::validate_path(path)?;
		// Not through a symlinked folder: what it reaches isn't the file put away.
		if crate::checkout::has_symlinked_ancestor(wt.work(), path) {
			continue;
		}
		match wt.work().remove_file(path) {
			Ok(()) => crate::checkout::remove_empty_parents(wt.work(), path),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => not_removed.push((path.clone(), error)),
		}
	}
	Ok(not_removed)
}

/// The top-most folders under `dir` the clean-up removes whole, added to `wholes`: untracked, in
/// scope, not a nested repository (nor under one that isn't), with every file under it put away and
/// every folder under it the same (an empty one included). Whether `dir` itself is such a folder's
/// contents, everything in it going.
fn plan_wholes<F, W, H>(
	wt: &WorkTree<F, W, H>,
	dir: &GitPath,
	saved: &HashSet<&GitPath>,
	tracked_folders: &HashSet<GitPath>,
	scope: Option<&PathspecSet>,
	wholes: &mut Vec<GitPath>,
) -> Result<bool, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let Ok(entries) = wt.work().read_dir(dir) else {
		return Ok(false);
	};
	let mut all_go = true;
	let mut inner = Vec::new();
	for entry in entries {
		if dir.is_root() && entry.name.as_bytes() == b".git" {
			all_go = false;
			continue;
		}
		let path = dir.join(&entry.name);
		if !entry.kind.is_dir() {
			all_go &= saved.contains(&path);
			continue;
		}
		let candidate = !tracked_folders.contains(&path)
			&& scope.is_none_or(|scope| scope.matches_directory(&path))
			&& !is_nested_repository(wt, &path)?;
		if candidate && plan_wholes(wt, &path, saved, tracked_folders, scope, &mut inner)? {
			inner.push(path);
		} else {
			all_go = false;
		}
	}
	if all_go && !dir.is_root() {
		// The caller lists this folder instead of what's in it.
		return Ok(true);
	}
	wholes.extend(inner);
	Ok(false)
}

/// Whether `path` holds a `.git` of its own (a nested repository, left alone).
fn is_nested_repository<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &GitPath,
) -> Result<bool, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	Ok(
		wt.work()
			.lstat(&path.join(&gitana_path::GitPathComponent::from_utf8(".git").expect(".git")))?
			.is_some(),
	)
}

/// Remove `folder` and what's in it, as git's `remove_dirs` does: its entries in the order the
/// filesystem lists them, each folder before itself, then the folder once everything in it has gone.
/// Only files put away are removed (anything else found there, and a nested repository, stays, and
/// so does the folder); each one that can't be is added to `not_removed`, and so are the folders it
/// leaves. Whether the folder went.
fn remove_whole<F, W, H>(
	wt: &WorkTree<F, W, H>,
	folder: &GitPath,
	saved: &HashSet<&GitPath>,
	not_removed: &mut Vec<(GitPath, std::io::Error)>,
) -> Result<bool, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let entries = match wt.work().read_dir(folder) {
		Ok(entries) => entries,
		// An unreadable folder may still be removable when empty, as git tries.
		Err(_) => {
			return match wt.work().remove_dir(folder) {
				Ok(()) => Ok(true),
				Err(error) => {
					not_removed.push((folder.clone(), error));
					Ok(false)
				}
			};
		}
	};
	let mut gone = true;
	for entry in entries {
		let path = folder.join(&entry.name);
		if entry.kind.is_dir() {
			gone &= !is_nested_repository(wt, &path)? && remove_whole(wt, &path, saved, not_removed)?;
			continue;
		}
		if !saved.contains(&path) {
			gone = false;
			continue;
		}
		match wt.work().remove_file(&path) {
			Ok(()) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => {
				not_removed.push((path, error));
				gone = false;
			}
		}
	}
	if !gone {
		return Ok(false);
	}
	match wt.work().remove_dir(folder) {
		Ok(()) => Ok(true),
		Err(error) => {
			not_removed.push((folder.clone(), error));
			Ok(false)
		}
	}
}

/// [`WorkTree::stash_apply`].
pub(crate) async fn apply<F, W, H>(
	wt: &WorkTree<F, W, H>,
	request: &StashApply,
) -> Result<StashApplied<H>, StashApplyError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	let entries = repo.stash_list().await?;
	if entries.is_empty() {
		return Err(StashApplyError::NoEntries);
	}
	let Some(entry) = entries.get(request.index) else {
		return Err(StashApplyError::OutOfRange {
			index: request.index,
			count: entries.len(),
		});
	};
	let w = entry.commit;
	let w_commit = read_commit(repo, w).await?;
	let [base, staged, rest @ ..] = w_commit.parents.as_slice() else {
		return Err(StashApplyError::NotStashLike(w.to_hex()));
	};
	let trees = StashTrees {
		base: repo.commit_tree(*base).await?,
		staged: repo.commit_tree(*staged).await?,
		work: w_commit.tree,
		untracked: match rest.first() {
			Some(untracked) => Some(repo.commit_tree(*untracked).await?),
			None => None,
		},
	};
	let lock = wt.lock_index().await?;
	let index_time = wt.index_time().await?;
	let mut index = wt.load_index().await?;
	match apply_locked(wt, &lock, index_time, &mut index, request, &trees).await {
		Ok(applied) => {
			wt.commit_index(lock, &index).await?;
			Ok(StashApplied {
				commit: w,
				conflicts: applied.conflicts,
				index_restored: applied.index_restored,
				replaced: applied.replaced,
				moved_aside: applied.moved_aside,
			})
		}
		Err(error) => {
			wt.release_index_lock(lock).await;
			Err(error)
		}
	}
}

/// A stash's trees: its base (`HEAD` when it was made), what was staged (`i`), the working files
/// (`w`) and the untracked ones (`u`).
struct StashTrees<H: HashAlgorithm> {
	base: ObjectId<H>,
	staged: ObjectId<H>,
	work: ObjectId<H>,
	untracked: Option<ObjectId<H>>,
}

/// Bring the stash back into `index` and the working tree under `lock`, as git's `do_apply_stash`
/// does: every refusal before the first write. The conflicted paths, and whether the staged
/// changes came back staged.
async fn apply_locked<F, W, H>(
	wt: &WorkTree<F, W, H>,
	lock: &IndexLock<'_, F>,
	index_time: Option<std::time::SystemTime>,
	index: &mut Index<H>,
	request: &StashApply,
	trees: &StashTrees<H>,
) -> Result<Applied, StashApplyError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	if index.has_conflicts() {
		return Err(StashApplyError::Unmerged);
	}
	let current = repo.write_tree(&index.tree_entries()).await?;
	// With `restore_index`, the stash's staged changes on the current index, unless there were none
	// (or they're already there); refused if they don't apply.
	let restored_index =
		if request.restore_index && trees.staged != trees.base && trees.staged != current {
			let merged = repo.merge_trees(trees.base, current, trees.staged).await?;
			if !merged.conflicts.is_empty() {
				return Err(StashApplyError::IndexConflicts);
			}
			Some(merged.tree)
		} else {
			None
		};
	let merge = repo
		.merge_trees_labelled(
			trees.base,
			current,
			trees.work,
			"Updated upstream",
			"Stashed changes",
		)
		.await?;
	// Every untracked file the stash holds must be free to create, checked before anything changes.
	let untracked = match trees.untracked {
		Some(tree) => repo.read_tree(tree).await?,
		None => Vec::new(),
	};
	let merged_entries = repo.read_tree(merge.tree).await?;
	let merged_paths: HashSet<&GitPath> = merged_entries.iter().map(|(path, _, _)| path).collect();
	let mut checkout = Converter::check_out(
		wt,
		merged_entries
			.iter()
			.chain(untracked.iter())
			.map(|(path, mode, oid)| (path, u32::from_str_radix(mode, 8).unwrap_or(0), *oid)),
	)
	.await?;
	let ours = crate::conflicted_state::tree_entry_map(repo, current).await?;
	// The files and folders that will exist once the merge is written, compared as the filesystem
	// compares names (folding case under `core.ignoreCase`), so an untracked file the filesystem
	// would see as one of them, or as one written before it (`Foo` and `foo` from a case-sensitive
	// machine), is refused rather than written over it.
	let fold = crate::excludes::ignore_case(wt).await?;
	let key = |path: &GitPath| {
		if fold {
			path.ascii_folded()
		} else {
			path.as_bytes().to_vec()
		}
	};
	// The tracked files the merge deletes, by the same keys: one standing where an untracked file
	// needs a folder is gone before untracked files are written.
	let merged_keys: HashSet<Vec<u8>> = merged_paths.iter().map(|path| key(path)).collect();
	let deleted_keys: HashSet<Vec<u8>> = ours
		.keys()
		.map(&key)
		.filter(|folded| !merged_keys.contains(folded))
		.collect();
	let mut planned_files: HashSet<Vec<u8>> = HashSet::new();
	let mut planned_folders: HashSet<Vec<u8>> = HashSet::new();
	for path in &merged_paths {
		planned_files.insert(key(path));
		planned_folders.extend(path.strict_ancestors().iter().map(&key));
	}
	for (path, mode, oid) in &untracked {
		crate::checkout::validate_path(path)?;
		// Its blob must be there to write, found now rather than once tracked files have changed.
		if mode != "160000" {
			repo.read_blob(*oid).await?;
		}
		wt.work()
			.validate_path_representable(path)
			.map_err(WorktreeError::from)?;
		let folders: Vec<Vec<u8>> = path.strict_ancestors().iter().map(&key).collect();
		let clashes = planned_files.contains(&key(path))
			|| planned_folders.contains(&key(path))
			|| folders.iter().any(|folder| planned_files.contains(folder));
		if clashes
			|| wt
				.work()
				.lstat(path)
				.map_err(WorktreeError::from)?
				.is_some()
			|| folder_blocked(wt, path, |folder| deleted_keys.contains(&key(folder)))?
		{
			return Err(StashApplyError::UntrackedExists(path.clone()));
		}
		checkout.check_write(path, mode).await?;
		planned_files.insert(key(path));
		planned_folders.extend(folders);
	}
	let base = crate::conflicted_state::tree_entry_map(repo, trees.base).await?;
	let theirs = crate::conflicted_state::tree_entry_map(repo, trees.work).await?;
	// A conflicted path's unstaged changes would be overwritten by its markers: refused, as the
	// merge-like operations refuse, before anything changes.
	if !merge.conflicts.is_empty() {
		let diverged = wt.diverged_tracked_content_paths().await?;
		let clobbered: Vec<GitPath> = merge
			.conflicts
			.iter()
			.filter(|path| {
				diverged.contains(*path) && !ours.get(*path).is_some_and(|(mode, _)| *mode == 0o160000)
			})
			.cloned()
			.collect();
		if !clobbered.is_empty() {
			return Err(WorktreeError::ConflictOverwrite(ConflictOverwriteError::new(clobbered)).into());
		}
		wt.check_materialise(merge.tree, &merge.conflicts).await?;
	}
	let before: HashMap<GitPath, IndexEntry<H>> = index
		.entries
		.iter()
		.filter(|entry| entry.stage == 0)
		.map(|entry| (entry.path.clone(), entry.clone()))
		.collect();
	// Paths outside the sparse checkout that the stash changes are written back with their
	// skip-worktree bit cleared, as git's merge writes them, so the stash's version isn't lost: where
	// the index left afterwards (the current one, or with `restore_index` the one restored) doesn't hold
	// it already. Outside: skip-worktree, or for a path new to the index, beyond the sparse patterns.
	// One with changes of its own in the working tree is refused by the merge below, as git refuses
	// local changes a merge would overwrite; all checked before anything is written.
	let sparse = wt.sparse_checkout().await?;
	let outside = |path: &GitPath| match before.get(path) {
		Some(entry) => entry.skip_worktree,
		None => sparse
			.as_ref()
			.is_some_and(|matcher| !matcher.includes(path)),
	};
	let restored_entries = match restored_index {
		Some(tree) => Some(crate::conflicted_state::tree_entry_map(repo, tree).await?),
		None => None,
	};
	let mut write_back: Vec<GitPath> = Vec::new();
	for (path, mode, oid) in &merged_entries {
		if merge.conflicts.contains(path) || !outside(path) {
			continue;
		}
		let version = (u32::from_str_radix(mode, 8).unwrap_or(0), *oid);
		let kept = match &restored_entries {
			Some(restored) => restored.get(path),
			None => ours.get(path),
		};
		if kept != Some(&version) {
			write_back.push(path.clone());
		}
	}
	// A deletion the stash holds of such a path comes back as one: the entry left afterwards no
	// longer skipped, so its missing file shows as deleted, as git leaves it.
	let mut unskip: Vec<GitPath> = write_back.clone();
	for path in restored_entries.as_ref().unwrap_or(&ours).keys() {
		if !merged_paths.contains(path) && !merge.conflicts.contains(path) && outside(path) {
			unskip.push(path.clone());
		}
	}
	// A file of the user's where a path new to the index is written back is moved aside first, as
	// git's merge moves it, to `<path>.stash.<6 random letters or digits>`.
	let mut moved_aside: Vec<(GitPath, GitPath)> = Vec::new();
	for path in &write_back {
		if !before.contains_key(path)
			&& wt
				.work()
				.lstat(path)
				.map_err(WorktreeError::from)?
				.is_some()
		{
			let aside = aside_name(wt, path, &merged_paths)?;
			moved_aside.push((path.clone(), aside));
		}
	}
	if !write_back.is_empty() {
		wt.check_materialise(merge.tree, &write_back).await?;
	}
	// The untracked files are created first, each only if its name is free as the filesystem judges
	// names, so a name it treats as one already there (a Unicode-equivalent spelling, say, which the
	// checks above can't see) is found. A taken name, or the merge's own refusal below, removes the
	// files and folders created here and refuses, nothing else changed. They change nothing the
	// index records, so they come before the lock is marked.
	let mut created = Created::new(wt);
	let mut after_merge = Vec::new();
	// The files the merge will write where nothing stands now (new to it, or changed and missing),
	// and the missing folders above them: one of those files there once the untracked files are
	// written, or one of those folders there as a file, is an untracked file under a spelling the
	// filesystem treats as the same (`é` composed and decomposed), which the merge would write over
	// or delete to make the folder (an ignored file being expendable to it), so it's refused first.
	// Each is `(path, whether it's a folder)`.
	let mut fresh: Vec<(GitPath, bool)> = Vec::new();
	let mut seen_folders: HashSet<GitPath> = HashSet::new();
	if !untracked.is_empty() {
		for (path, mode, oid) in &merged_entries {
			let mode = u32::from_str_radix(mode, 8).unwrap_or(0);
			let unchanged = ours
				.get(path)
				.is_some_and(|(current_mode, current_oid)| *current_mode == mode && current_oid == oid);
			if unchanged {
				continue;
			}
			for folder in path.strict_ancestors() {
				if !folder.is_root()
					&& seen_folders.insert(folder.clone())
					&& wt
						.work()
						.lstat(&folder)
						.map_err(WorktreeError::from)?
						.is_none()
				{
					fresh.push((folder, true));
				}
			}
			if wt
				.work()
				.lstat(path)
				.map_err(WorktreeError::from)?
				.is_none()
			{
				fresh.push((path.clone(), false));
			}
		}
	}
	// A folder an untracked file needs that the merge also creates for a tracked file (by the same
	// keys, `N/u` and `n/new` where case folds) is made first, in the merge's spelling, as git,
	// merging first, spells it.
	let fresh_folders: HashMap<Vec<u8>, &GitPath> = fresh
		.iter()
		.filter(|(_, folder)| *folder)
		.map(|(path, _)| (key(path), path))
		.collect();
	if !fresh_folders.is_empty() {
		for (path, _, _) in &untracked {
			for folder in path.strict_ancestors() {
				let Some(merged) = (!folder.is_root())
					.then(|| fresh_folders.get(&key(&folder)))
					.flatten()
				else {
					continue;
				};
				if wt
					.work()
					.lstat(merged)
					.map_err(WorktreeError::from)?
					.is_some()
				{
					continue;
				}
				for above in merged.strict_ancestors() {
					if !above.is_root()
						&& wt
							.work()
							.lstat(&above)
							.map_err(WorktreeError::from)?
							.is_none()
					{
						created.folders.push(above);
					}
				}
				if wt
					.work()
					.populate_new(merged, gitana_file_store_local::PopulationEntry::Directory)
					.map_err(WorktreeError::from)?
				{
					created.folders.push((*merged).clone());
				}
			}
		}
	}
	for (path, mode, oid) in &untracked {
		// One that needs a folder where a tracked file stands (the merge deletes it, as checked
		// above) is created just after the merge instead.
		let mut in_a_file = false;
		for folder in path.strict_ancestors() {
			if wt
				.work()
				.lstat(&folder)
				.map_err(WorktreeError::from)?
				.is_some_and(|meta| !meta.kind.is_dir())
			{
				in_a_file = true;
				break;
			}
		}
		if in_a_file {
			after_merge.push((path, mode, oid));
			continue;
		}
		match create_untracked(wt, &mut checkout, path, mode, *oid, &mut created).await {
			Ok(true) => {}
			Ok(false) => {
				created.remove();
				return Err(StashApplyError::UntrackedExists(path.clone()));
			}
			Err(error) => {
				created.remove();
				return Err(error.into());
			}
		}
	}
	for (path, folder) in &fresh {
		if wt
			.work()
			.lstat(path)
			.map_err(WorktreeError::from)?
			.is_some_and(|meta| !(*folder && meta.kind.is_dir()))
		{
			// Named by the untracked file that's there, when it's one of those written.
			let there = wt
				.work()
				.entry_identity(path)
				.map_err(WorktreeError::from)?;
			let named = created
				.files
				.iter()
				.find(|(_, identity)| there.as_ref() == Some(identity))
				.map_or(path, |(untracked, _)| untracked)
				.clone();
			created.remove();
			return Err(StashApplyError::UntrackedExists(named));
		}
	}
	// The merge into the working tree and index; its own refusals (local changes it would overwrite,
	// a file in the way) come before its first write.
	if let Err(error) = crate::checkout::merge_apply_locked(
		wt,
		lock,
		index_time,
		index,
		current,
		merge.tree,
		request.excludes_file.as_deref(),
		false,
	)
	.await
	{
		created.remove();
		return Err(error.into());
	}
	// Those where a deleted tracked file stood: a name taken even so (only by another of the
	// stash's own files, the folder being new) is refused with the merge applied, the stash kept.
	for (path, mode, oid) in after_merge {
		if !create_untracked(wt, &mut checkout, path, mode, *oid, &mut created).await? {
			return Err(StashApplyError::UntrackedExists(path.clone()));
		}
	}
	lock.mark_mutation_started();
	let mut index_restored = false;
	let mut replaced: Vec<GitPath> = Vec::new();
	if !merge.conflicts.is_empty() {
		// As the merge-like operations leave a conflict: the markers written whatever the sparse
		// patterns, and the stages recorded (base, the current index, the stash).
		let merged = crate::conflicted_state::tree_entry_map(repo, merge.tree).await?;
		let force_gitlink_mounts: HashSet<GitPath> = merge
			.conflicts
			.iter()
			.filter(|path| {
				merged.get(*path).is_some_and(|(mode, _)| *mode == 0o160000)
					&& merged.get(*path) != ours.get(*path)
			})
			.cloned()
			.collect();
		wt.materialise_paths(merge.tree, &merge.conflicts, &force_gitlink_mounts)
			.await?;
		for path in &merge.conflicts {
			index.record_conflict(
				path,
				base.get(path).copied(),
				ours.get(path).copied(),
				theirs.get(path).copied(),
			);
		}
	} else if let Some(tree) = restored_index {
		// What was staged comes back staged: the index as the stash's staged changes on the current one.
		let entries: Vec<IndexEntry<H>> = repo
			.read_tree(tree)
			.await?
			.into_iter()
			.map(|(path, mode, oid)| {
				let mode = u32::from_str_radix(&mode, 8).unwrap_or(0o100644);
				match index.entry(&path) {
					Some(entry) if entry.mode == mode && entry.oid == oid => entry.clone(),
					// The bit the merge reached, or for a path new to the index the sparse rule: a
					// path outside the sparse checkout stays skipped, as git's does (unless written
					// back below).
					current => {
						let skip_worktree = current
							.map(|entry| entry.skip_worktree)
							.unwrap_or_else(|| outside(&path));
						let mut entry = new_entry(path, mode, oid);
						entry.skip_worktree = skip_worktree;
						entry
					}
				}
			})
			.collect();
		*index = Index::new();
		for entry in entries {
			index.upsert(entry);
		}
		index_restored = true;
	} else {
		// git's default: the changes come back unstaged, except a file new to the index, which stays
		// staged so it isn't left untracked.
		let merged_index: Vec<IndexEntry<H>> = index
			.entries
			.iter()
			.filter(|entry| entry.stage == 0)
			.cloned()
			.collect();
		// The skip-worktree bit the merge reached for each path (it sets it on a clean file outside the
		// sparse checkout it removes), kept on the entry restored.
		let reconciled: HashMap<GitPath, (bool, u32, ObjectId<H>)> = merged_index
			.iter()
			.map(|entry| {
				(
					entry.path.clone(),
					(entry.skip_worktree, entry.mode, entry.oid),
				)
			})
			.collect();
		let added: Vec<IndexEntry<H>> = merged_index
			.into_iter()
			.filter(|entry| !before.contains_key(&entry.path))
			.collect();
		let mut kept: Vec<&IndexEntry<H>> = before.values().collect();
		kept.sort_by(|left, right| left.path.cmp(&right.path));
		*index = Index::new();
		for entry in kept {
			// A new file or folder takes the place of the entries it collides with, as git leaves
			// it (reporting each).
			if added
				.iter()
				.any(|new| new.path.is_below(&entry.path) || entry.path.is_below(&new.path))
			{
				replaced.push(entry.path.clone());
				continue;
			}
			let mut entry = entry.clone();
			match reconciled.get(&entry.path) {
				Some(&(skip_worktree, mode, oid)) => {
					entry.skip_worktree = skip_worktree;
					// The merge wrote a different file there: the old stat no longer describes it,
					// so it's compared by content, as git's reset leaves such an entry.
					if (mode, oid) != (entry.mode, entry.oid) {
						entry.stat = Stat::default();
					}
				}
				// The merge removed the file.
				None => entry.stat = Stat::default(),
			}
			index.upsert(entry);
		}
		for entry in added {
			index.upsert(entry);
		}
	}
	for (path, aside) in &moved_aside {
		wt.work().rename(path, aside).map_err(WorktreeError::from)?;
	}
	if !write_back.is_empty() {
		wt.materialise_paths(merge.tree, &write_back, &HashSet::new())
			.await?;
	}
	for entry in index.entries.iter_mut() {
		if entry.stage == 0 && unskip.contains(&entry.path) {
			entry.skip_worktree = false;
		}
	}
	created.keep();
	Ok(Applied {
		conflicts: merge.conflicts,
		index_restored,
		replaced,
		moved_aside,
	})
}

/// Whether something in the working tree stands where one of `path`'s folders must be: a file (or
/// symlink), unless it's a tracked file the merge deletes (`deleted`, compared as the filesystem
/// compares names), which goes before untracked files are written (the merge refusing first if it
/// has local changes).
fn folder_blocked<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &GitPath,
	deleted: impl Fn(&GitPath) -> bool,
) -> Result<bool, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	for folder in path.strict_ancestors() {
		match wt.work().lstat(&folder)? {
			Some(meta) if meta.kind.is_dir() => {}
			Some(_) => return Ok(!deleted(&folder)),
			None => return Ok(false),
		}
	}
	Ok(false)
}

/// The untracked files and folders a pop created, removed again when it refuses, and when it's
/// dropped unfinished (a cancelled pop leaves none of them behind), until [`Created::keep`]. Each
/// file goes only while it's still the one written, unchanged, and each folder only while empty.
struct Created<'w, F: FileStore, W: WorkDirFs, H: HashAlgorithm> {
	wt: &'w WorkTree<F, W, H>,
	files: Vec<(GitPath, WorkEntryIdentity)>,
	folders: Vec<GitPath>,
	kept: bool,
}

impl<'w, F: FileStore, W: WorkDirFs, H: HashAlgorithm> Created<'w, F, W, H> {
	fn new(wt: &'w WorkTree<F, W, H>) -> Self {
		Self {
			wt,
			files: Vec::new(),
			folders: Vec::new(),
			kept: false,
		}
	}

	/// The pop succeeded: what was created stays.
	fn keep(mut self) {
		self.kept = true;
	}

	/// Remove what was created, files first, then folders deepest first, as far as each file is
	/// still the one written, unchanged, and each folder is empty.
	fn remove(&mut self) {
		for (path, identity) in self.files.drain(..) {
			let _ = self.wt.work().remove_file_if_identity(&path, &identity);
		}
		self
			.folders
			.sort_by_key(|folder| std::cmp::Reverse(folder.as_bytes().len()));
		self.folders.dedup();
		for folder in self.folders.drain(..) {
			let _ = self.wt.work().remove_dir(&folder);
		}
	}
}

impl<F: FileStore, W: WorkDirFs, H: HashAlgorithm> Drop for Created<'_, F, W, H> {
	fn drop(&mut self) {
		if !self.kept {
			self.remove();
		}
	}
}

/// Create the untracked file `path` from the stash, only if its name is free as the filesystem
/// judges names; whether it was created. The folders made for it are recorded in `created`.
async fn create_untracked<F, W, H>(
	wt: &WorkTree<F, W, H>,
	checkout: &mut Converter<'_, F, W, H>,
	path: &GitPath,
	mode: &str,
	oid: ObjectId<H>,
	created: &mut Created<'_, F, W, H>,
) -> Result<bool, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	crate::checkout::validate_path(path)?;
	for folder in path.strict_ancestors() {
		if !folder.is_root() && wt.work().lstat(&folder)?.is_none() {
			created.folders.push(folder);
		}
	}
	let content = wt.repository().read_blob(oid).await?;
	// Recorded as created, so a rollback removes it only while it's still this file, unchanged: one
	// another program replaces or rewrites meanwhile is left alone.
	let recorded = if mode == "120000" {
		wt.work().populate_new_recorded(
			path,
			gitana_file_store_local::PopulationEntry::Symlink(&content),
		)?
	} else {
		let content = checkout.convert_to_worktree(path, &content).await?;
		wt.work().populate_new_recorded(
			path,
			gitana_file_store_local::PopulationEntry::Regular {
				bytes: &content,
				executable: mode == "100755",
			},
		)?
	};
	let Some(identity) = recorded else {
		return Ok(false);
	};
	created.files.push((path.clone(), identity));
	Ok(true)
}

/// What [`apply_locked`] did, for [`StashApplied`].
struct Applied {
	conflicts: Vec<GitPath>,
	index_restored: bool,
	replaced: Vec<GitPath>,
	moved_aside: Vec<(GitPath, GitPath)>,
}

/// A free name beside `path` for a file of the user's moved out of the way, as git names it:
/// `<path>.stash.<6 random letters or digits>`, taken by nothing in the working tree or `taken`.
fn aside_name<F, W, H>(
	wt: &WorkTree<F, W, H>,
	path: &GitPath,
	taken: &HashSet<&GitPath>,
) -> Result<GitPath, WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	use std::hash::BuildHasher;
	const CHARACTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
	let state = std::collections::hash_map::RandomState::new();
	let mut attempt: u64 = 0;
	loop {
		let mut seed = state.hash_one((path.as_bytes(), attempt));
		let mut name = path.as_bytes().to_vec();
		name.extend_from_slice(b".stash.");
		for _ in 0..6 {
			name.push(CHARACTERS[(seed % 62) as usize]);
			seed /= 62;
		}
		let candidate =
			GitPath::from_bytes(name).map_err(|error| WorktreeError::InvalidPath(error.to_string()))?;
		if !taken.contains(&candidate) && wt.work().lstat(&candidate)?.is_none() {
			return Ok(candidate);
		}
		attempt += 1;
	}
}

/// An index entry for `path` at `mode` and `oid`, with no stat (so it's compared by content).
fn new_entry<H: HashAlgorithm>(path: GitPath, mode: u32, oid: ObjectId<H>) -> IndexEntry<H> {
	IndexEntry {
		stat: Stat::default(),
		mode,
		oid,
		stage: 0,
		assume_valid: false,
		skip_worktree: false,
		intent_to_add: false,
		path,
	}
}

/// [`WorkTree::stash_pop`].
// The error carries what was applied when the drop fails, as `StashPopError` was agreed.
#[allow(clippy::result_large_err)]
pub(crate) async fn pop<F, W, H>(
	wt: &WorkTree<F, W, H>,
	request: &StashApply,
) -> Result<StashApplied<H>, StashPopError<H>>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let applied = apply(wt, request).await?;
	if applied.conflicts.is_empty() {
		match wt
			.repository()
			.drop_stash(request.index, Some(applied.commit))
			.await
		{
			Ok(Some(_)) => {}
			// Nothing at its place any more (another stash dropped meanwhile): kept, as for one
			// moved.
			Ok(None) => {
				return Err(StashPopError::Drop {
					applied,
					error: RepositoryError::RefMoved {
						name: "refs/stash".to_owned(),
					},
				});
			}
			Err(error) => return Err(StashPopError::Drop { applied, error }),
		}
	}
	Ok(applied)
}
