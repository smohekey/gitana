//! `gta clone` — copy a repository from a Git Smart HTTP remote.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use gitana_object::{HashAlgorithm, HashKind, Sha1, Sha256};
use gitana_porcelain::{Deepen, Identity};
use gitana_remote::{
	self as transport, Connection, HttpConnection, LocalConnection, Origin, RemoteUrl, SshConnection,
};

use crate::identity::CliIdentity;
use crate::shallow::build_deepen;
use crate::{CloneDestination, CommandContext, git_config, repo, transport_for, url_rewrite};

/// Clone the repository at `url` into `dir` (default: the repo slug). Anonymous: works
/// for public repos. The local repository is created in whatever object format the
/// remote advertises.
///
/// `depth` / `shallow_since` / `shallow_exclude` request a shallow clone (git's `--depth`,
/// `--shallow-since`, `--shallow-exclude`): a truncated history recorded in `.git/shallow`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
	command: &CommandContext,
	url: String,
	dir: Option<PathBuf>,
	depth: Option<u32>,
	shallow_since: Option<String>,
	shallow_exclude: Vec<String>,
	sparse: bool,
	recurse_submodules: Vec<String>,
	shallow_submodules: bool,
	remote_submodules: bool,
) -> Result<()> {
	// Fail fast on a bad `--shallow-since` before any network round-trip.
	let deepen = build_deepen(depth, shallow_since.as_deref(), shallow_exclude)?;
	let process_cwd = std::env::current_dir()?;
	let command_cwd = git_config::command_cwd().unwrap_or_else(|| process_cwd.clone());
	let naming_cwd = if command_cwd.is_absolute() {
		command_cwd.clone()
	} else {
		process_cwd.join(&command_cwd)
	};
	// Apply `url.*.insteadOf` before parsing, from the ambient (global/system) config — there is no local
	// config yet. git rewrites the transport URL this way (so a `git@…`-style alias could even map to
	// https). The default checkout directory comes from the *original* argument (git's `guess_dir_name`),
	// not the rewritten URL, in case a rewrite changes the last path segment.
	let config = git_config::from_ambient().await?;
	let rewritten_url = url_rewrite::rewrite_fetch_url(&config, &url)?;
	let remote = RemoteUrl::parse(&rewritten_url)?;
	command.authorize(
		&config,
		&remote,
		gitana_remote::ProtocolContext::UserInitiated,
	)?;
	let target_argument = dir.unwrap_or_else(|| default_directory_path(&url, &naming_cwd));
	// `-C` changes the effective working directory for the whole command. Resolve both an explicit
	// relative destination and the inferred directory there before any no-follow classification or
	// publication; otherwise the source is interpreted under `-C` while the destination leaks back to
	// the process launch directory.
	let target = if target_argument.is_absolute() {
		target_argument
	} else {
		naming_cwd.join(target_argument)
	};
	// Classify the destination without following symlinks. In particular, `Path::exists` reports a
	// dangling symlink as absent; treating it as attempt-owned would let failure cleanup unlink an
	// artifact that predates the clone. Only a real empty directory is eligible for reuse.
	let target_preexisting = match std::fs::symlink_metadata(&target) {
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
			if target.read_dir()?.next().transpose()?.is_some() {
				bail!(
					"destination path '{}' already exists and is not empty",
					target.display()
				);
			}
			true
		}
		Ok(_) => {
			bail!(
				"destination path '{}' already exists and is not an empty directory",
				target.display()
			);
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
		Err(error) => return Err(error.into()),
	};

	// Git normally records `clone: from <url>` using the URL verbatim (with userinfo stripped); a
	// direct relative local path is the exception and is resolved to its canonical absolute source.
	// Start with the ordinary spelling here and replace it in the local transport arm when needed.
	let reflog_url = transport::anonymize_url(&url);
	// Record the ORIGINAL clone argument in `remote.origin.url` (not the `insteadOf`-rewritten transport
	// URL), so a later change to the rewrite rules still applies on subsequent fetches. The *original
	// spelling* is preserved — including a trailing slash the rewrite prefix may depend on and an SSH /
	// scp-like alias. A direct relative local path is again resolved absolutely. Gitana additionally
	// redacts any password (git persists it verbatim; gitana deliberately never writes a plaintext
	// credential to `.git/config`, as on a plain userinfo clone).
	let persist_url = transport::redact_password(&url);
	// The directory external helpers run from — git's effective working directory (`gta`'s `-C`, or the
	// launch dir): where a relative askpass (HTTP) or a relative `GIT_SSH_COMMAND` / key path (SSH)
	// resolves. There is no worktree yet, so it is the launch/`-C` directory, matching git.
	let mut destination = CloneDestination::new(&target, target_preexisting);

	// Open the transport as a connection, negotiate the object format from the advertisement, then run
	// the porcelain clone over it — one code path for both HTTP and SSH.
	let clone_result: Result<()> = async {
		match remote {
			RemoteUrl::Http(origin) => {
				// Credentials resolve from the ambient (global/system) config plus any URL userinfo — there is
				// no local config yet — and the one transport carries them through the advertisement GET and the
				// pack POST alike.
				let http = transport_for(config, &origin, command_cwd)?;
				let body = transport::fetch_advertisement(&http, &origin, "git-upload-pack").await?;
				let kind = transport::negotiated_kind(&body)?;
				let worktree = destination.start()?;
				let git_dir = worktree.join(".git");
				create_skeleton(&git_dir)?;
				let mut connection = HttpConnection::new(
					&http,
					origin.upload_pack(),
					transport::UPLOAD_PACK_REQUEST,
					body,
				);
				clone_over(
					&mut connection,
					kind,
					&git_dir,
					&worktree,
					&deepen,
					&reflog_url,
					&persist_url,
					sparse,
				)
				.await?;
			}
			RemoteUrl::Ssh(ssh) => {
				// SSH sends the ref advertisement on connect — no separate GET — so opening the connection
				// yields it directly. gitana drives the user's `ssh` (resolved from git's `GIT_SSH_COMMAND` /
				// `core.sshCommand` / `GIT_SSH` precedence and variant), run from the effective command
				// directory so a relative command / key resolves as git's would.
				let ssh_cmd = crate::ssh::resolve_ssh_command(&config)?;
				let mut connection =
					SshConnection::open(&ssh, "git-upload-pack", &ssh_cmd, &command_cwd).await?;
				let kind = transport::negotiated_kind(connection.advertisement())?;
				let worktree = destination.start()?;
				let git_dir = worktree.join(".git");
				create_skeleton(&git_dir)?;
				clone_over(
					&mut connection,
					kind,
					&git_dir,
					&worktree,
					&deepen,
					&reflog_url,
					&persist_url,
					sparse,
				)
				.await?;
			}
			RemoteUrl::Local(path) => {
				let source = local_path(&command_cwd, &path);
				let found = repo::inspect_root(&source).await?;
				let source_identity = repo::capture_repository_layout_identity(&found)?;
				let (local_reflog_url, local_persist_url) =
					if rewritten_url == url && Path::new(&path).is_relative() {
						let absolute = repo::local_source_url(&found)?;
						(absolute.clone(), absolute)
					} else {
						(reflog_url.clone(), persist_url.clone())
					};
				let local_deepen = if rewritten_url.starts_with("file://") {
					deepen.clone()
				} else {
					Deepen::default()
				};
				let (source_setup, common, git) =
					repo::revalidated_local_source_setup(&found, source_identity, None).await?;
				let kind = crate::dispatch::detect_algorithm_at(&common, &found.common_dir).await?;
				match kind {
					HashKind::Sha1 => {
						let source =
							repo::open_generic_from_dirs::<Sha1>(common, git, &found.git_dir, &found.common_dir)
								.await?;
						drop(source_setup);
						let worktree = destination.start()?;
						let git_dir = worktree.join(".git");
						create_skeleton(&git_dir)?;
						let mut connection = LocalConnection::open(source).await?;
						clone_over(
							&mut connection,
							kind,
							&git_dir,
							&worktree,
							&local_deepen,
							&local_reflog_url,
							&local_persist_url,
							sparse,
						)
						.await?;
					}
					HashKind::Sha256 => {
						let source = repo::open_generic_from_dirs::<Sha256>(
							common,
							git,
							&found.git_dir,
							&found.common_dir,
						)
						.await?;
						drop(source_setup);
						let worktree = destination.start()?;
						let git_dir = worktree.join(".git");
						create_skeleton(&git_dir)?;
						let mut connection = LocalConnection::open(source).await?;
						clone_over(
							&mut connection,
							kind,
							&git_dir,
							&worktree,
							&local_deepen,
							&local_reflog_url,
							&local_persist_url,
							sparse,
						)
						.await?;
					}
				}
			}
		}
		Ok(())
	}
	.await;
	if let Err(error) = clone_result {
		if let Err(cleanup) = destination.cleanup_after_failure() {
			return Err(error.context(format!(
				"cleaning failed clone destination '{}': {cleanup}",
				target.display()
			)));
		}
		return Err(error);
	}
	let (published_root, published_identity) = destination.commit_repository()?;

	// Report the userinfo-stripped URL — a password in the clone URL must not reach stdout / CI logs.
	println!("Cloned '{}' into '{}'", reflog_url, target.display());
	if !recurse_submodules.is_empty() {
		let layout = repo::inspect_root(&published_root).await?;
		let credential_url_base = (transport::redact_password(&url) != url).then_some(url);
		super::submodule::update_published_clone(
			layout,
			published_identity,
			command,
			recurse_submodules,
			credential_url_base,
			shallow_submodules.then_some(1),
			remote_submodules,
		)
		.await?;
	}
	Ok(())
}

fn local_path(cwd: &Path, path: &str) -> PathBuf {
	let path = PathBuf::from(path);
	if path.is_absolute() {
		path
	} else {
		cwd.join(path)
	}
}

/// Create the git directory skeleton, like `init`.
fn create_skeleton(git_dir: &Path) -> Result<()> {
	for sub in [
		"objects/pack",
		"objects/info",
		"refs/heads",
		"refs/tags",
		"info",
	] {
		std::fs::create_dir_all(git_dir.join(sub))?;
	}
	Ok(())
}

/// Run the porcelain clone over `connection`, dispatching the repository's hash algorithm (negotiated
/// from the advertisement) so the rest is generic over `H`.
#[allow(clippy::too_many_arguments)]
async fn clone_over(
	connection: &mut impl Connection,
	kind: HashKind,
	git_dir: &Path,
	target: &Path,
	deepen: &Deepen,
	reflog_url: &str,
	persist_url: &str,
	sparse: bool,
) -> Result<()> {
	match kind {
		HashKind::Sha1 => {
			clone_as::<Sha1>(
				connection,
				git_dir,
				target,
				deepen,
				reflog_url,
				persist_url,
				sparse,
			)
			.await
		}
		HashKind::Sha256 => {
			clone_as::<Sha256>(
				connection,
				git_dir,
				target,
				deepen,
				reflog_url,
				persist_url,
				sparse,
			)
			.await
		}
	}
}

#[allow(clippy::too_many_arguments)]
async fn clone_as<H: HashAlgorithm>(
	connection: &mut impl Connection,
	git_dir: &Path,
	target: &Path,
	deepen: &Deepen,
	reflog_url: &str,
	persist_url: &str,
	sparse: bool,
) -> Result<()> {
	let repo = repo::open_generic::<H>(git_dir, git_dir).await?;
	// The committer falls back to a placeholder when unconfigured, as git's reflog writes do.
	let committer = CliIdentity::new(&repo).committer_or_default().await?;
	gitana_porcelain::clone(
		connection,
		repo,
		repo::open_work_dir(target)?,
		deepen,
		Some(gitana_porcelain::CloneReflog {
			committer: &committer,
			url: reflog_url,
		}),
		persist_url,
		sparse,
	)
	.await
}

/// The default checkout directory for the *original* clone argument (git's `guess_dir_name`). Local
/// remotes use the native path parser so `.` names the command directory, a terminal `.git` names its
/// repository parent, and Windows separators remain path separators. URL and rewrite-alias spellings
/// retain the existing URL-oriented slug behavior.
fn default_directory_path(url: &str, cwd: &Path) -> PathBuf {
	match RemoteUrl::parse(url) {
		Ok(RemoteUrl::Local(path)) => default_local_directory_path(cwd, &path),
		_ => PathBuf::from(default_remote_directory_name(url)),
	}
}

fn default_local_directory_path(cwd: &Path, path: &str) -> PathBuf {
	if let Some(dot) = trailing_dot_component(path) {
		return PathBuf::from(dot);
	}
	let mut source = lexical_normalize(&local_path(cwd, path));
	if source.file_name().is_some_and(is_dot_git) {
		source.pop();
	}
	let Some(name) = source.file_name() else {
		return PathBuf::from("repository");
	};
	let name = strip_git_suffix(name).unwrap_or_else(|| name.to_os_string());
	if name.is_empty() {
		PathBuf::from("repository")
	} else {
		PathBuf::from(name)
	}
}

fn trailing_dot_component(path: &str) -> Option<&str> {
	let trimmed = if cfg!(windows) {
		path.trim_end_matches(['/', '\\'])
	} else {
		path.trim_end_matches('/')
	};
	let component = if cfg!(windows) {
		trimmed.rsplit(['/', '\\']).next()
	} else {
		trimmed.rsplit('/').next()
	}?;
	matches!(component, "." | "..").then_some(component)
}

fn lexical_normalize(path: &Path) -> PathBuf {
	let mut normalized = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				normalized.pop();
			}
			_ => normalized.push(component.as_os_str()),
		}
	}
	normalized
}

fn is_dot_git(name: &OsStr) -> bool {
	if cfg!(windows) {
		name
			.to_str()
			.is_some_and(|name| name.eq_ignore_ascii_case(".git"))
	} else {
		name == OsStr::new(".git")
	}
}

fn strip_git_suffix(name: &OsStr) -> Option<OsString> {
	let path = Path::new(name);
	let extension = path.extension()?;
	let is_git = if cfg!(windows) {
		extension
			.to_str()
			.is_some_and(|extension| extension.eq_ignore_ascii_case("git"))
	} else {
		extension == OsStr::new("git")
	};
	if !is_git {
		return None;
	}
	let stem = path.file_stem()?;
	(!stem.is_empty()).then(|| stem.to_os_string())
}

fn default_remote_directory_name(url: &str) -> String {
	if let Ok(origin) = Origin::parse(url) {
		return origin.directory_name();
	}
	let last = url
		.trim_end_matches('/')
		.rsplit(['/', ':'])
		.find(|segment| !segment.is_empty())
		.unwrap_or("repository");
	let name = last.strip_suffix(".git").unwrap_or(last);
	if name.is_empty() {
		"repository".to_owned()
	} else {
		name.to_owned()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_dir_from_original_url() {
		let cwd = std::env::current_dir().unwrap().join("work/project");
		assert_eq!(
			default_directory_path("https://alias/input.git", &cwd),
			PathBuf::from("input")
		);
		assert_eq!(
			default_directory_path("https://host/a/b/", &cwd),
			PathBuf::from("b")
		);
		// A non-http (scp-like) alias still yields a sensible slug.
		assert_eq!(
			default_directory_path("git@host:org/repo.git", &cwd),
			PathBuf::from("repo")
		);
	}

	#[test]
	fn default_dir_from_native_local_path() {
		let cwd = std::env::current_dir().unwrap().join("work/project");
		assert_eq!(default_directory_path(".", &cwd), PathBuf::from("."));
		assert_eq!(default_directory_path("./", &cwd), PathBuf::from("."));
		assert_eq!(default_directory_path("source/.", &cwd), PathBuf::from("."));
		assert_eq!(
			default_directory_path("source/./", &cwd),
			PathBuf::from(".")
		);
		assert_eq!(default_directory_path("..", &cwd), PathBuf::from(".."));
		assert_eq!(default_directory_path("../", &cwd), PathBuf::from(".."));
		assert_eq!(
			default_directory_path("source/..", &cwd),
			PathBuf::from("..")
		);
		assert_eq!(
			default_directory_path("../source.git", &cwd),
			PathBuf::from("source")
		);
		assert_eq!(
			default_directory_path(&cwd.join(".git").to_string_lossy(), &cwd),
			PathBuf::from("project")
		);
	}

	#[cfg(unix)]
	#[test]
	fn default_dir_from_local_file_url() {
		let cwd = Path::new("/work/project");
		assert_eq!(
			default_directory_path("file:///srv/team/project/.git", cwd),
			PathBuf::from("project")
		);
	}

	#[cfg(windows)]
	#[test]
	fn default_dir_from_windows_native_path() {
		let cwd = Path::new(r"C:\work\project");
		assert_eq!(
			default_directory_path(r"C:\source\repository.git", cwd),
			PathBuf::from("repository")
		);
		assert_eq!(
			default_directory_path(r"C:\source\repository\.git", cwd),
			PathBuf::from("repository")
		);
	}

	#[tokio::test]
	async fn recursive_handoff_rejects_a_replaced_published_root() {
		let temporary = tempfile::tempdir().unwrap();
		let target = temporary.path().join("target");
		let retained = temporary.path().join("retained");
		let mut destination = CloneDestination::new(&target, false);
		let staging = destination.start().unwrap();
		std::fs::create_dir_all(staging.join(".git/refs")).unwrap();
		let (published_root, identity) = destination.commit_repository().unwrap();

		std::fs::rename(&published_root, &retained).unwrap();
		std::fs::create_dir_all(published_root.join(".git/refs")).unwrap();
		let layout = repo::inspect_root(&published_root).await.unwrap();
		let command = CommandContext::from_env(temporary.path().to_owned(), Vec::new());
		let error = crate::commands::submodule::update_published_clone(
			layout,
			identity,
			&command,
			vec![".".to_owned()],
			None,
			None,
			false,
		)
		.await
		.unwrap_err();

		assert!(
			error
				.to_string()
				.contains("changed while waiting for repository setup"),
			"unexpected replacement error: {error:#}"
		);
		assert!(
			published_root.join(".git/refs").is_dir(),
			"the replacement repository must remain untouched"
		);
		assert!(
			retained.join(".git/refs").is_dir(),
			"the published clone must remain recoverable at its displaced name"
		);
	}
}
