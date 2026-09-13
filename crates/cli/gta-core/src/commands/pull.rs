//! `gta pull` — fetch from the origin (updating remote-tracking refs) and integrate the current
//! branch's upstream: fast-forward, or a true merge commit when the histories have diverged.

use std::path::Path;

use anyhow::{Context, Result, bail};
use cap_std::fs::Dir;
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::Identity;
use gitana_remote::{
	self as transport, Connection, HttpPackFetcher, LocalConnection, LocalPackFetcher, PackFetcher,
	RemoteUrl, SshConnection, SshPackFetcher,
};
use gitana_repository::HeadState;
use gitana_submodule::WorktreeMutationGuard;
use gitana_worktree::WorkTree;

use crate::commands::merge;
use crate::dispatch;
use crate::identity::CliIdentity;
use crate::signer;
use crate::{
	CommandContext, RepositoryLayoutIdentity, RetainedCommandDirectory, WorkDir, git_config, repo,
	transport_for, url_rewrite,
};

/// Pull `HEAD`'s branch from the origin.
pub async fn run(cwd: &Path) -> Result<()> {
	let cwd = tokio::fs::canonicalize(cwd).await?;
	let found = repo::discover(&cwd).await?;
	let command_directory = RetainedCommandDirectory::capture(cwd.clone()).await?;
	let identity = repo::capture_worktree_layout_identity(&found)?;
	// The origin URL is `remote.origin.url` with `url.*.insteadOf` applied, read from the merged config.
	let (setup, common, git, _) = repo::command_setup_lease(&found, identity).await?;
	let config = git_config::for_worktree_at(common, git, &found.common_dir, &found.git_dir).await?;
	drop(setup);
	let url = url_rewrite::resolve_fetch_url(&config, "origin")?;
	let remote = RemoteUrl::parse(&url)?;
	if let Some(command) = CommandContext::current() {
		command.authorize(
			&config,
			&remote,
			gitana_remote::ProtocolContext::UserInitiated,
		)?;
	}
	// A credential-free form for the "Fetched from" line and the merge commit message — *all* userinfo
	// stripped (a token can occupy the username field), so no credential can reach a persisted commit.
	// The raw `url` is only for the auth-bearing transport parse above.
	let display = transport::anonymize_url(&url);
	// A relative askpass (HTTP) / `GIT_SSH_COMMAND` (SSH) resolves against the worktree root, as git runs
	// it from there (bare: git dir).
	let askpass_cwd = found
		.worktree_root
		.clone()
		.unwrap_or_else(|| found.common_dir.clone());

	match remote {
		RemoteUrl::Http(origin) => {
			let http = transport_for(config, &origin, askpass_cwd)?;
			let body = transport::fetch_advertisement(&http, &origin, "git-upload-pack").await?;
			let mut fetcher = HttpPackFetcher::new(&http, &origin);
			pull_dispatch(
				&mut fetcher,
				&found,
				identity,
				&body,
				&display,
				command_directory,
			)
			.await
		}
		RemoteUrl::Ssh(ssh) => {
			let ssh_cmd = crate::ssh::resolve_ssh_command(&config)?;
			let connection = SshConnection::open(&ssh, "git-upload-pack", &ssh_cmd, &askpass_cwd).await?;
			let body = connection.advertisement().to_vec();
			let mut fetcher = SshPackFetcher::new(connection);
			pull_dispatch(
				&mut fetcher,
				&found,
				identity,
				&body,
				&display,
				command_directory,
			)
			.await
		}
		RemoteUrl::Local(path) => {
			let source = {
				let path = std::path::PathBuf::from(path);
				if path.is_absolute() {
					path
				} else {
					askpass_cwd.join(path)
				}
			};
			let source_layout = repo::inspect_root(&source).await?;
			let source_identity = repo::capture_repository_layout_identity(&source_layout)?;
			let (source_setup, common, git) =
				repo::revalidated_local_source_setup(&source_layout, source_identity, None).await?;
			match dispatch::detect_algorithm_at(&common, &source_layout.common_dir).await? {
				HashKind::Sha1 => {
					let second_common = common.try_clone()?;
					let second_git = git.try_clone()?;
					let source = repo::open_generic_from_dirs::<Sha1>(
						common,
						git,
						&source_layout.git_dir,
						&source_layout.common_dir,
					)
					.await?;
					let connection = LocalConnection::open(source).await?;
					let body = connection.advertisement().to_vec();
					let source = repo::open_generic_from_dirs::<Sha1>(
						second_common,
						second_git,
						&source_layout.git_dir,
						&source_layout.common_dir,
					)
					.await?;
					drop(source_setup);
					let mut fetcher = LocalPackFetcher::new(source);
					pull_dispatch(
						&mut fetcher,
						&found,
						identity,
						&body,
						&display,
						command_directory,
					)
					.await
				}
				HashKind::Sha256 => {
					let second_common = common.try_clone()?;
					let second_git = git.try_clone()?;
					let source = repo::open_generic_from_dirs::<Sha256>(
						common,
						git,
						&source_layout.git_dir,
						&source_layout.common_dir,
					)
					.await?;
					let connection = LocalConnection::open(source).await?;
					let body = connection.advertisement().to_vec();
					let source = repo::open_generic_from_dirs::<Sha256>(
						second_common,
						second_git,
						&source_layout.git_dir,
						&source_layout.common_dir,
					)
					.await?;
					drop(source_setup);
					let mut fetcher = LocalPackFetcher::new(source);
					pull_dispatch(
						&mut fetcher,
						&found,
						identity,
						&body,
						&display,
						command_directory,
					)
					.await
				}
			}
		}
	}
}

/// Negotiate the object format from the advertisement, then run the per-hash pull over `fetcher`.
async fn pull_dispatch(
	fetcher: &mut impl PackFetcher,
	found: &repo::RepositoryLayout,
	identity: RepositoryLayoutIdentity,
	body: &[u8],
	url: &str,
	command_directory: RetainedCommandDirectory,
) -> Result<()> {
	let (setup, common, _, _) = repo::command_setup_lease(found, identity).await?;
	let local = dispatch::detect_algorithm_at(&common, &found.common_dir).await?;
	drop(setup);
	transport::ensure_same_format(local, transport::negotiated_kind(body)?)?;
	match local {
		HashKind::Sha1 => {
			pull_into::<Sha1>(fetcher, found, identity, body, url, command_directory).await
		}
		HashKind::Sha256 => {
			pull_into::<Sha256>(fetcher, found, identity, body, url, command_directory).await
		}
	}
}

/// Fetch into the remote-tracking refs, then merge the current branch's upstream tip. Both are the
/// porcelain composites; this composes them, printing the "Fetched from" line *between* — so a merge
/// that then fails (e.g. a dirty work tree) still reports the completed fetch, as git does.
async fn pull_into<H: HashAlgorithm>(
	fetcher: &mut impl PackFetcher,
	found: &repo::RepositoryLayout,
	identity: RepositoryLayoutIdentity,
	body: &[u8],
	url: &str,
	command_directory: RetainedCommandDirectory,
) -> Result<()> {
	let work = found
		.worktree_root
		.clone()
		.context("cannot pull in a bare repository")?;
	let (setup, common, git, work_dir) = repo::command_setup_lease(found, identity).await?;
	let work_dir = work_dir.context("cannot pull in a bare repository")?;
	let repository =
		repo::open_generic_from_dirs::<H>(common, git, &found.git_dir, &found.common_dir).await?;
	let bare = repo::repository_bare_snapshot(&repository).await?;
	drop(setup);
	let worktree = WorkTree::new_located(
		repository,
		WorkDir::from_dir(work_dir),
		found.git_dir.clone(),
		work.clone(),
	);

	// Every branch checked out in a worktree. `update_head_ok` below exempts only *this* worktree's
	// branch (advanced by the merge); a branch checked out in another worktree is still refused, since
	// pull's merge advances only this worktree's HEAD.
	let checkouts = repo::branch_checkouts(&found.common_dir, bare)
		.into_iter()
		.map(|(branch, path)| (branch, path.display().to_string()))
		.collect::<Vec<_>>();
	// `update_head_ok`: a fetch refspec may map straight into the checked-out branch (a mirror config
	// like `+refs/heads/*:refs/heads/*`); the merge below advances that branch and the work tree.
	// Under a pull, git reflogs the tracking-ref updates with the `pull` action (not `fetch`), honouring
	// `GIT_REFLOG_ACTION` if set. The merge step below records HEAD/branch separately; this covers only
	// the tracking refs.
	let committer = CliIdentity::new(worktree.repository())
		.committer_or_default()
		.await?;
	let action = crate::identity::reflog_action("pull");
	let outcome = gitana_porcelain::fetch_with_bare(
		fetcher,
		worktree.repository(),
		bare,
		body,
		true,
		gitana_porcelain::TagFetch::Auto,
		false, // pull does not prune (git prunes only under an explicit --prune)
		&gitana_porcelain::Deepen::default(),
		&checkouts,
		Some(gitana_porcelain::FetchReflog {
			committer: &committer,
			action: &action,
		}),
	)
	.await?;
	println!("Fetched from {url}");
	// A rejected (non-fast-forward) tracking update is a failed fetch; do not merge a stale upstream.
	if !outcome.rejected.is_empty() {
		bail!("some remote-tracking refs were not updated (non-fast-forward)");
	}

	// The upstream tip is the current branch's remote branch, read straight from the advertisement —
	// the merge source, whatever tracking ref (if any) the fetch refspecs routed it to.
	let branch = match worktree.repository().refs().read_head().await? {
		HeadState::Symbolic(branch) => branch,
		HeadState::Detached(_) => bail!("cannot pull onto a detached HEAD"),
	};
	let short = branch.strip_prefix("refs/heads/").unwrap_or(&branch);
	let upstream = gitana_porcelain::pull_upstream(worktree.repository(), body, &branch)
		.await?
		.with_context(|| format!("origin has no {short} to merge (or a refspec excludes it)"))?;
	let message = format!("Merge branch '{short}' of {url}");
	drop(worktree);

	// Fetch must not retain shared-config serialization across network I/O. Reacquire it only for
	// integration, then bind it to the repository backend so a cancelled merge cannot release the
	// lease while a detached config or checkout worker can still observe the Windows publication gap.
	let (worktree, command_path, cwd_directory, mutation_guard) =
		open_merge_worktree::<H>(found, identity, &work, command_directory).await?;
	let identity = CliIdentity::new(worktree.repository());

	// A pull's merge commit is signed when git config requests it, like a plain `gta merge`.
	let signer =
		signer::config_signer_in(worktree.repository(), &command_path, &cwd_directory).await?;
	let outcome = gitana_porcelain::merge(
		&worktree,
		&upstream.to_hex(),
		Some(message),
		false,
		false,
		&identity,
		signer.as_ref(),
	)
	.await;
	mutation_guard.validate()?;
	let outcome = outcome?;
	merge::render(outcome)
}

/// Reopen the pull target for post-fetch integration under worker-bound config serialization.
async fn open_merge_worktree<H: HashAlgorithm>(
	found: &repo::RepositoryLayout,
	identity: RepositoryLayoutIdentity,
	work: &Path,
	command_directory: RetainedCommandDirectory,
) -> Result<(
	WorkTree<crate::Backend, crate::WorkDir, H>,
	std::path::PathBuf,
	Dir,
	WorktreeMutationGuard,
)> {
	let (guard, setup, common, git, work_dir) =
		repo::command_worktree_mutation_lease(found, identity)
			.await
			.with_context(|| format!("pull worktree changed during fetch: {}", work.display()))?;
	let work_dir =
		work_dir.with_context(|| format!("pull worktree changed during fetch: {}", work.display()))?;
	let repo::RevalidatedCommandDirectory {
		path: command_path,
		directory: cwd_directory,
		common,
		git,
		worktree,
	} = repo::revalidated_command_directory(
		found,
		command_directory,
		&setup,
		common,
		git,
		Some(work_dir),
	)
	.await?;
	let work_dir = worktree.expect("pull supplied the worktree directory");
	let repository = repo::open_generic_from_dirs_with_worker_lease::<H>(
		common,
		git,
		&found.git_dir,
		&found.common_dir,
		setup,
	)
	.await?;
	Ok((
		WorkTree::new_located(
			repository,
			WorkDir::from_dir(work_dir),
			found.git_dir.clone(),
			work.to_owned(),
		),
		command_path,
		cwd_directory,
		guard,
	))
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
	use std::sync::mpsc::{RecvTimeoutError, channel};
	use std::time::Duration;

	use cap_std::{ambient_authority, fs::Dir};
	use gitana_object::Sha1;
	use gitana_submodule::acquire_submodule_config_mutation_lease;

	use super::open_merge_worktree;
	use crate::RetainedCommandDirectory;
	use crate::repo;

	#[tokio::test]
	async fn merge_worktree_retains_setup_serialization_until_dropped() {
		let temporary = tempfile::tempdir().unwrap();
		let temporary_root = std::fs::canonicalize(temporary.path()).unwrap();
		let work = temporary_root.join("work");
		let git = work.join(".git");
		std::fs::create_dir_all(git.join("objects")).unwrap();
		std::fs::create_dir_all(git.join("refs")).unwrap();
		std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(
			git.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		let layout = repo::inspect_root(&work).await.unwrap();
		let identity = repo::capture_worktree_layout_identity(&layout).unwrap();
		let cwd = std::fs::canonicalize(&work).unwrap();
		let command_directory = RetainedCommandDirectory::capture(cwd).await.unwrap();
		let (worktree, _command_path, _cwd_directory, mutation_guard) =
			open_merge_worktree::<Sha1>(&layout, identity, &work, command_directory)
				.await
				.unwrap();

		let (started_sender, started_receiver) = channel();
		let (acquired_sender, acquired_receiver) = channel();
		let mutation_git = git.clone();
		let mutation = std::thread::spawn(move || {
			let common = Dir::open_ambient_dir(&mutation_git, ambient_authority()).unwrap();
			started_sender.send(()).unwrap();
			let lease = acquire_submodule_config_mutation_lease(&common, &mutation_git).unwrap();
			acquired_sender.send(()).unwrap();
			lease
		});
		started_receiver.recv().unwrap();
		assert!(matches!(
			acquired_receiver.recv_timeout(Duration::from_millis(50)),
			Err(RecvTimeoutError::Timeout)
		));

		drop(worktree);
		drop(mutation_guard);
		acquired_receiver
			.recv_timeout(Duration::from_secs(1))
			.expect("merge worktree must release setup serialization when dropped");
		drop(mutation.join().unwrap());
	}

	#[tokio::test]
	async fn merge_worktree_rejects_a_public_checkout_replaced_during_fetch() {
		let temporary = tempfile::tempdir().unwrap();
		let temporary_root = std::fs::canonicalize(temporary.path()).unwrap();
		let work = temporary_root.join("work");
		let retained = temporary_root.join("retained");
		let git = temporary_root.join("module.git");
		std::fs::create_dir_all(&work).unwrap();
		std::fs::create_dir_all(git.join("objects")).unwrap();
		std::fs::create_dir_all(git.join("refs")).unwrap();
		std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(
			git.join("config"),
			"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
		)
		.unwrap();
		std::fs::write(work.join(".git"), "gitdir: ../module.git\n").unwrap();
		let found = repo::inspect_root(&work).await.unwrap();
		let identity = repo::capture_worktree_layout_identity(&found).unwrap();

		std::fs::rename(&work, &retained).unwrap();
		std::fs::create_dir(&work).unwrap();
		std::fs::write(work.join(".git"), "gitdir: ../module.git\n").unwrap();

		let command_directory = RetainedCommandDirectory::capture(work.clone())
			.await
			.unwrap();
		let error = match open_merge_worktree::<Sha1>(&found, identity, &work, command_directory).await
		{
			Ok(_) => panic!("a replaced public checkout must not be reopened for integration"),
			Err(error) => error,
		};
		assert!(
			error
				.to_string()
				.contains("pull worktree changed during fetch"),
			"unexpected error: {error:#}"
		);
		assert_eq!(
			std::fs::read(work.join(".git")).unwrap(),
			b"gitdir: ../module.git\n"
		);
		assert!(retained.join(".git").is_file());
	}
}
