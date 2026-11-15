//! `switch <branch>`: move the working tree, index and `HEAD` to a branch — git's two-tree merge
//! from the current `HEAD` (carrying local work that doesn't conflict), or, forced, an authoritative
//! checkout — optionally making the branch first (`switch -c`).

use std::collections::HashSet;
use std::path::Path;

use gitana_file_store::{FileStore, FileStoreError};
use gitana_file_store_local::WorkDirFs;
use gitana_identity::Identity;
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::{GitPath, GitPathComponent};
use gitana_repository::{BranchName, ReflogIntent, Repository, RepositoryError};

use crate::fsmeta::{join_rel, push_gitignore};
use crate::ignore::{self, DirIgnore};
use crate::{CheckoutMode, Index, SwitchError, SwitchOptions, WorkTree, WorktreeError};

pub(crate) async fn run<F, W, H>(
	wt: &WorkTree<F, W, H>,
	name: &BranchName,
	options: &SwitchOptions<'_>,
	identity: &impl Identity,
) -> Result<(), SwitchError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	let branch = name.full();
	let committer = identity
		.committer_or_default()
		.await
		.map_err(SwitchError::Identity)?;

	// Hold this worktree's `HEAD.lock` for the whole checkout, from before the merge base is read
	// through publishing the new `HEAD`, so a concurrent `HEAD` retarget or a ref transaction moving
	// the branch checked out here can't interleave with it. It releases on drop if the checkout
	// fails or is cancelled first, leaving `HEAD` unmoved.
	let head_lock = repo.refs().lock_head().await?;
	// What `HEAD` points at now, for the reflog's `from`, read under the lock.
	let from = head_description(repo).await?;

	// The branch-already-exists check comes before any tree work, as git does it; the branch itself is
	// made only after a successful checkout (below), so a failed checkout leaves no ref behind.
	let target = match options.create {
		Some(start) => {
			if repo.refs().resolve(&branch).await?.is_some() {
				return Err(SwitchError::Exists(name.clone()));
			}
			let named = repo.rev_parse(start.unwrap_or("HEAD")).await?;
			repo.peel_to_commit(named).await?
		}
		None => match repo.refs().resolve(&branch).await? {
			Some(commit) => commit,
			None => return Err(SwitchError::NoSuchBranch(name.clone())),
		},
	};

	// A branch's ref is shared by a repository's worktrees, so git forbids checking one branch out in
	// two of them (their commits would race on the ref). Refused before the working tree is touched.
	if let Some(place) = checked_out_elsewhere(wt, &branch).await? {
		return Err(SwitchError::CheckedOutElsewhere {
			branch: name.clone(),
			place,
		});
	}

	// git refuses to move `HEAD` while a merge, cherry-pick, revert or rebase is unconcluded, even
	// forced, so its state isn't left attached to another branch.
	if let Some(operation) = repo.operation_in_progress().await? {
		return Err(SwitchError::InProgress(operation));
	}

	let tree = repo.commit_tree(target).await?;
	if options.force {
		wt.checkout(tree, true, options.excludes_file).await?;
	} else {
		// The tree the index matches now: `HEAD`'s, or the empty tree for an unborn `HEAD`, so staged
		// work on an orphan branch is carried across too, as git does.
		let head_tree = match repo.refs().resolve_head().await? {
			Some(commit) => repo.commit_tree(commit).await?,
			None => repo.write_tree(&[]).await?,
		};
		// Told not to overwrite ignored files, the checkout checks for them itself, under its index
		// lock.
		crate::checkout::run_with(
			wt,
			tree,
			CheckoutMode::Merge { head: head_tree },
			options.excludes_file,
			!options.overwrite_ignored,
		)
		.await?;
	}

	// git records the start point as named, defaulting to the literal `HEAD` for `switch -c`. The
	// create asserts the branch's absence, so one made meanwhile fails here rather than being
	// overwritten.
	let created_from = options.create.flatten().unwrap_or("HEAD");
	let create_message = format!("branch: Created from {created_from}");
	let create = options.create.is_some().then(|| {
		(
			target,
			ReflogIntent::Log {
				committer: &committer,
				message: &create_message,
			},
		)
	});
	// Publish by consuming the `HEAD.lock` held across the checkout: make the branch (if creating) and
	// set `HEAD` in one owned task that keeps the lock.
	let checkout_message = format!("checkout: moving from {from} to {name}");
	head_lock
		.finish_checkout(
			&branch,
			create,
			ReflogIntent::Log {
				committer: &committer,
				message: &checkout_message,
			},
		)
		.await?;
	Ok(())
}

/// `HEAD` for a checkout reflog: the current branch's short name, or `HEAD`'s full object id when
/// detached.
async fn head_description<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<String, RepositoryError> {
	match repo.refs().read_symbolic("HEAD").await? {
		Some(target) => Ok(
			target
				.strip_prefix("refs/heads/")
				.unwrap_or(&target)
				.to_owned(),
		),
		None => Ok(repo.rev_parse("HEAD").await?.to_hex()),
	}
}

/// Where else `branch` is checked out among the repository's worktrees, if anywhere: the main one
/// (unless the repository is bare) or a linked one, other than `wt`'s own. Read through the file
/// store (each linked worktree's `worktrees/<id>/HEAD`, and the main one's `HEAD` from the shared
/// directory), so it works wherever the store does. Named as the worktree's folder where it can be
/// told, else as the main worktree or the linked one's id.
async fn checked_out_elsewhere<F, W, H>(
	wt: &WorkTree<F, W, H>,
	branch: &str,
) -> Result<Option<String>, SwitchError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let store = wt.repository().objects().file_store();
	let own = wt.git_dir();
	// This is a linked worktree if its git dir holds a `commondir` file, as git decides it; the store
	// reads `commondir` from the worktree's own git dir. A linked worktree's git dir is then
	// `<common>/worktrees/<id>`. The main one's is the common dir, wherever that is.
	let linked = match store.read_path("commondir").await {
		Ok(_) => true,
		Err(FileStoreError::NotFound) => false,
		Err(error) => return Err(SwitchError::Repository(error.into())),
	};
	// This worktree's own admin entry, to leave out: by its git folder's name, compared by bytes
	// (an admin folder's name needn't be UTF-8). A worktree built from store capabilities alone has
	// no such name; its entry is then the one whose `gitdir` names the same checkout, the store
	// reading this worktree's own `gitdir` from its admin folder.
	let own_id = own
		.file_name()
		.filter(|_| linked)
		.map(|id| id.as_encoded_bytes().to_vec());
	let own_gitdir = if linked && own_id.is_none() {
		match store.read_path("gitdir").await {
			Ok(bytes) => Some(bytes),
			Err(FileStoreError::NotFound) => None,
			Err(error) => return Err(SwitchError::Repository(error.into())),
		}
	} else {
		None
	};
	let points_at = |bytes: &[u8]| {
		std::str::from_utf8(bytes)
			.ok()
			.and_then(|head| head.strip_prefix("ref:"))
			.is_some_and(|to| to.trim() == branch)
	};
	// The main worktree, if this is a linked one, and its `HEAD` names the branch, and it isn't
	// bare: its bareness is read only then, so a main-worktree config it can't read doesn't stop a
	// switch to another branch.
	if linked {
		match store.read_shared_path("HEAD").await {
			Ok(bytes) if points_at(&bytes) && !main_is_bare(wt).await? => {
				let common = own.parent().and_then(Path::parent);
				let main = common.map(|common| match common.file_name() {
					Some(name) if name == ".git" => common.parent().unwrap_or(common),
					_ => common,
				});
				return Ok(Some(main.map_or_else(
					|| "the main worktree".to_owned(),
					|path| path.display().to_string(),
				)));
			}
			Ok(_) | Err(FileStoreError::NotFound) => {}
			Err(error) => return Err(SwitchError::Repository(error.into())),
		}
	}
	// Listed and read by bytes, so a linked worktree whose admin folder's name isn't UTF-8 is
	// checked too; named lossily only in the message.
	let component = |name: &str| GitPathComponent::from_utf8(name).expect("a valid path component");
	let worktrees = GitPath::root().join(&component("worktrees"));
	let listed = match store.list_prefix_bytes(&worktrees).await {
		Ok(listed) => listed,
		Err(FileStoreError::NotFound) => Vec::new(),
		Err(error) => return Err(SwitchError::Repository(error.into())),
	};
	for id in listed {
		if own_id.as_deref() == Some(id.as_bytes()) {
			continue;
		}
		let admin = worktrees.join(&id);
		// A stray file there (a `.DS_Store`, say) is no worktree; git passes over it too.
		if !store
			.is_dir_bytes(&admin)
			.await
			.map_err(|error| SwitchError::Repository(error.into()))?
		{
			continue;
		}
		let gitdir = store
			.read_path_bytes(&admin.join(&component("gitdir")))
			.await
			.ok();
		if own_gitdir.is_some() && gitdir == own_gitdir {
			continue;
		}
		match store.read_path_bytes(&admin.join(&component("HEAD"))).await {
			Ok(bytes) if points_at(&bytes) => {
				let named = || format!("worktree {}", String::from_utf8_lossy(id.as_bytes()));
				let place = match gitdir {
					Some(gitdir) => String::from_utf8_lossy(&gitdir)
						.trim()
						.strip_suffix("/.git")
						.map_or_else(named, str::to_owned),
					None => named(),
				};
				return Ok(Some(place));
			}
			Ok(_) | Err(FileStoreError::NotFound) => {}
			Err(error) => return Err(SwitchError::Repository(error.into())),
		}
	}
	Ok(None)
}

/// Whether the main worktree is bare, so its `HEAD` checks nothing out, as git decides it from a
/// linked worktree: `core.bare` is true in this worktree's repository config, or in the main
/// worktree's own `config.worktree` (which a linked worktree's config doesn't include). That file
/// counts only while `extensions.worktreeConfig` is on: git reads a leftover one even with it off,
/// which can put two worktrees on one branch, so gitana doesn't. Nor does it count when it has an
/// include (which could set `core.bare` back) or can't be read. And the main worktree's own
/// configuration must agree (its `config.worktree`, else the shared `config`), so this worktree's
/// own `core.bare` can't free the main one's branch; git refuses there too. Only
/// repository config counts: a system, global or command-line `core.bare` can't make the main
/// worktree's branch free (gitana keeps `core.bare` local; see
/// `docs/plans/core-config-global-aware.md`). An invalid value is an error, as git's is.
async fn main_is_bare<F, W, H>(wt: &WorkTree<F, W, H>) -> Result<bool, SwitchError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let repo = wt.repository();
	let store = repo.objects().file_store();
	let config = repo.effective_config().await?;
	let here = config
		.get_repository_bool("core", None, "bare")
		.map_err(WorktreeError::from)?;
	let per_worktree = config
		.get_repository_bool("extensions", None, "worktreeconfig")
		.map_err(WorktreeError::from)?;
	// What the main worktree's own `config.worktree` says of `core.bare`, while it counts: per-worktree
	// config on, readable, and include-free. One the store can't read (a symlink out of the git
	// folder, say), or with an include (which could set `core.bare` back, and git's check doesn't
	// follow), doesn't free the branch.
	let main_says = if per_worktree == Some(true) {
		match store.read_shared_path("config.worktree").await {
			Ok(bytes) => {
				// Values needn't be UTF-8 (a path, say); `core.bare` is read without requiring the rest to be.
				let main = gitana_config::GitConfigBytes::parse(&bytes).map_err(WorktreeError::from)?;
				let includes = main.entries().iter().any(|(key, _)| {
					let key = key.to_ascii_lowercase();
					key.starts_with(b"include.") || key.starts_with(b"includeif.")
				});
				if includes {
					return Ok(false);
				}
				main
					.get_bool("core", None, "bare")
					.map_err(WorktreeError::from)?
			}
			Err(FileStoreError::NotFound) => None,
			Err(_) => return Ok(false),
		}
	} else {
		None
	};
	// git's rule: this worktree's config says bare, or the main worktree's own `config.worktree` does.
	if here != Some(true) && main_says != Some(true) {
		return Ok(false);
	}
	// And the main worktree's own configuration must agree: its `config.worktree`, where that sets
	// `core.bare`, else the shared `config`. This worktree's own config doesn't speak for the main one.
	if let Some(bare) = main_says {
		return Ok(bare);
	}
	let shared = match store.read_shared_path("config").await {
		Ok(bytes) => bytes,
		Err(_) => return Ok(false),
	};
	let shared = gitana_config::GitConfigBytes::parse(&shared).map_err(WorktreeError::from)?;
	let bare = shared
		.get_bool("core", None, "bare")
		.map_err(WorktreeError::from)?;
	Ok(bare == Some(true))
}

/// That a checkout of `target` with no index file, which writes every path the target has,
/// overwrites or removes no ignored file, as git's `--no-overwrite-ignore` refuses to: see
/// [`refuse_ignored_writes`]. Called by the checkout once it holds the index lock, with the index it
/// loaded: `index`.
pub(crate) async fn refuse_ignored<F, W, H>(
	wt: &WorkTree<F, W, H>,
	target: ObjectId<H>,
	excludes_file: Option<&[u8]>,
	index: &Index<H>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let target = wt.repository().read_tree(target).await?;
	refuse_ignored_writes(
		wt,
		target
			.iter()
			.map(|(path, mode, _)| (path, mode == "160000")),
		excludes_file,
		index,
	)
	.await
}

/// That writing `writes` (each path, and whether the target has a submodule there) overwrites or
/// removes no ignored file: an untracked ignored file at the path, on a folder slot on its way, or
/// inside a folder the write puts a file in place of. A path the sparse-checkout leaves out isn't
/// materialised, so whatever is there is left alone, as git leaves it. The two-tree merge passes
/// the writes its plan holds, once the index lock is held; `index` is the index it loaded.
pub(crate) async fn refuse_ignored_writes<'a, F, W, H>(
	wt: &WorkTree<F, W, H>,
	writes: impl IntoIterator<Item = (&'a GitPath, bool)>,
	excludes_file: Option<&[u8]>,
	index: &Index<H>,
) -> Result<(), WorktreeError>
where
	F: FileStore,
	W: WorkDirFs,
	H: HashAlgorithm,
{
	let fold = crate::excludes::ignore_case(wt).await?;
	let key = |path: &GitPath| {
		if fold {
			path.ascii_folded()
		} else {
			path.as_bytes().to_vec()
		}
	};
	let tracked: HashSet<Vec<u8>> = index
		.entries
		.iter()
		.filter(|entry| entry.stage == 0)
		.map(|entry| key(&entry.path))
		.collect();
	let base = crate::excludes::load_base(wt, excludes_file).await?;
	let sparse = wt.sparse_checkout().await?;
	let work = wt.work();
	for (path, gitlink) in writes {
		if sparse
			.as_ref()
			.is_some_and(|matcher| !matcher.includes(path))
		{
			continue;
		}
		for ancestor in path.strict_ancestors() {
			match work.lstat(&ancestor).map_err(WorktreeError::from)? {
				Some(meta) if meta.kind.is_dir() => {}
				Some(_)
					if !tracked.contains(&key(&ancestor))
						&& crate::checkout::path_ignored(work, &ancestor, &base, fold)? =>
				{
					return Err(WorktreeError::IgnoredOverwrite(ancestor));
				}
				_ => break,
			}
		}
		if tracked.contains(&key(path)) {
			continue;
		}
		match work.lstat(path).map_err(WorktreeError::from)? {
			// Where the target puts a submodule, the checkout reuses whatever folder is there and keeps
			// what's in it, as git does, so nothing in it is overwritten.
			Some(meta) if meta.kind.is_dir() && gitlink => {}
			Some(meta) if meta.kind.is_dir() => {
				let mut stack = crate::checkout::ignore_prefix(work, path, &base)?;
				let above = path
					.strict_ancestors()
					.into_iter()
					.any(|ancestor| ignore::is_ignored_fold(&ancestor, true, &stack, fold));
				if let Some(ignored) =
					first_ignored_under(work, path, above, &tracked, &mut stack, fold, &key)?
				{
					return Err(WorktreeError::IgnoredOverwrite(ignored));
				}
			}
			Some(_) if crate::checkout::path_ignored(work, path, &base, fold)? => {
				return Err(WorktreeError::IgnoredOverwrite(path.clone()));
			}
			_ => {}
		}
	}
	Ok(())
}

/// The first untracked, ignored file under the working-tree folder `dir_rel`: what replacing the
/// folder with a file would remove. `ignored` is whether a folder above it is ignored; an ignored
/// folder makes everything in it ignored, but one whose files are all tracked loses nothing. `stack`
/// is the ignore stack down to `dir_rel`'s parent.
fn first_ignored_under<W: WorkDirFs>(
	work: &W,
	dir_rel: &GitPath,
	ignored: bool,
	tracked: &HashSet<Vec<u8>>,
	stack: &mut Vec<DirIgnore>,
	fold: bool,
	key: &impl Fn(&GitPath) -> Vec<u8>,
) -> Result<Option<GitPath>, WorktreeError> {
	let ignored = ignored || ignore::is_ignored_fold(dir_rel, true, stack, fold);
	// Inside an ignored folder everything is ignored already, so its own rules needn't be read.
	let pushed = !ignored && push_gitignore(work, dir_rel, stack)?;
	let mut found = None;
	for entry in work.read_dir(dir_rel)? {
		let rel = join_rel(dir_rel, &entry.name);
		// A nested repository's `.git` in an ignored folder is ignored content too, its history with
		// it: git's `--no-overwrite-ignore` won't lose it either. In a folder that isn't ignored it's
		// the checkout's to refuse, as untracked content.
		if entry.name == ".git" {
			if ignored {
				found = Some(rel);
				break;
			}
			continue;
		}
		if entry.kind.is_dir() {
			if let Some(hit) = first_ignored_under(work, &rel, ignored, tracked, stack, fold, key)? {
				found = Some(hit);
				break;
			}
		} else if !tracked.contains(&key(&rel))
			&& (ignored || ignore::is_ignored_fold(&rel, false, stack, fold))
		{
			found = Some(rel);
			break;
		}
	}
	if pushed {
		stack.pop();
	}
	Ok(found)
}
