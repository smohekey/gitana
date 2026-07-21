//! Discovery of git's configuration files and their precedence.
//!
//! The engine's [`Repository::read_config`] reads only the repository-local `.git/config` — it holds
//! a capability scoped to the git directory and cannot reach the user's `~/.gitconfig` or the system
//! `/etc/gitconfig`. Layering those in is a frontend concern (ambient path I/O), so it lives here.
//!
//! [`effective_config_at`] assembles git's full precedence stack — system, then global (XDG then
//! `~/.gitconfig`, or a single `$GIT_CONFIG_GLOBAL`), then repo-local — into one layered
//! [`GitConfig`]. Reads resolve across every layer (git's last-writer-wins); writes stay directed at
//! the repository-local file. The [`ConfigScope`] helpers resolve the single file `gta config
//! --global` / `--system` read and write, honouring the same `GIT_CONFIG_*` environment git does.
//!
//! These **merged** reads also expand git's `[include]` / `includeIf` directives (each layer's
//! includes spliced in at their position), threading the real gitdir and current branch through so
//! `includeIf "gitdir:"` / `"onbranch:"` / `"hasconfig:remote.*.url:"` resolve as git's do — including
//! git's whole-config remote-URL pre-scan and its paradox guard ([`expand_layers`]). An explicitly
//! *scoped* single-file read (`gta config --global`/`--system`/`--local`) does **not** expand
//! includes, matching git.

use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use gitana_config::{ConfigError, GitConfig, GitConfigSource, IncludeContext, IncludeResolver};
use tokio::io::AsyncWriteExt;

tokio::task_local! {
	/// The directory the command operates in (the `-C` directory, else the process cwd), established
	/// once at the CLI edge by [`with_command_cwd`]. A relative `$GIT_CONFIG_GLOBAL` / `$GIT_CONFIG_SYSTEM`
	/// resolves against it, matching git — which resolves such overrides against the directory it runs
	/// in, not the process's launch directory.
	static COMMAND_CWD: PathBuf;
}

/// Run `future` with `cwd` established as the command's working directory (see [`COMMAND_CWD`]). The
/// CLI front-ends wrap their whole dispatch in this so config-file overrides given as relative paths
/// resolve the way git resolves them under `-C`.
pub async fn with_command_cwd<F: Future>(cwd: PathBuf, future: F) -> F::Output {
	COMMAND_CWD.scope(cwd, future).await
}

/// The command's effective working directory ([`COMMAND_CWD`]), if established — for resolving a
/// relative program/path (e.g. a `core.askPass` helper) against the `-C` directory as git does,
/// since gitana records `-C` in this task-local rather than changing the process cwd.
pub(crate) fn command_cwd() -> Option<PathBuf> {
	COMMAND_CWD.try_with(|cwd| cwd.clone()).ok()
}

/// Which git configuration file a `gta config` operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
	/// The repository-local `.git/config` (git's default).
	Local,
	/// The user's global config (`$GIT_CONFIG_GLOBAL`, or `~/.config/git/config` / `~/.gitconfig`).
	Global,
	/// The system-wide config (`$GIT_CONFIG_SYSTEM`, or `/etc/gitconfig`).
	System,
}

/// The merged configuration for the repository whose per-worktree git directory is `git_dir` and whose
/// shared files live under `common`: the local `<common>/config` underlaid with the global and system
/// files in git's precedence order (system < global < local), with `[include]`/`includeIf` expanded,
/// then the `-c`/`GIT_CONFIG_*` command-line entries overlaid. Reads see every layer; writes and
/// [`GitConfig::render`] stay on the local file. `git_dir`'s real path and `HEAD` branch drive
/// `includeIf "gitdir:"`/`"onbranch:"` (the two are the same path for a non-linked repo). A repo whose
/// local config does not exist yet (a fresh `init`/`clone`) still resolves from the global and system
/// layers; a global/system/local file that exists but is malformed or unreadable is an error, as git
/// aborts on a bad config file.
///
/// This is the single merged-config assembler: [`open_generic`](crate::repo::open_generic) installs its
/// result on the repository, and the remote commands (which resolve credentials before opening the repo)
/// call it directly with the discovered layout's `git_dir`/`common_dir`.
pub async fn effective_config_at(git_dir: &Path, common: &Path) -> Result<GitConfig> {
	let gitdir = canonical_gitdir(git_dir).await;
	let gitdir_absolute = logical_gitdir(&gitdir).await;
	let branch = head_branch(git_dir).await;
	let mut base = global_and_system_layers().await?;
	base.push(local_layer(common).await?);
	assemble_merged(
		base,
		Some(&gitdir),
		gitdir_absolute.as_deref(),
		branch.as_deref(),
	)
	.await
}

/// The effective configuration for the **invoking worktree**: the merged stack ([`effective_config_at`])
/// with that worktree's `config.worktree` layered in when `extensions.worktreeConfig` is enabled. git's
/// precedence is `system < global < local < config.worktree < command-scope` — the per-worktree file sits
/// **below** the `-c`/`GIT_CONFIG_*` layer, not above it — so a worktree-scoped key like `core.ignorecase`
/// is overridable per worktree yet still yields to a command-line `-c`. Every layer is validated: a
/// malformed `extensions.worktreeConfig`, or an unreadable/unparseable `config.worktree`, is an error (git
/// aborts on a bad config file); only an *absent* `config.worktree` is skipped. `common` holds the shared
/// config (where `extensions.worktreeConfig` lives); `git_dir` is the invoking worktree's git dir.
///
/// `[include]`/`includeIf` directives are expanded across all layers (including `config.worktree`), as in
/// [`effective_config_at`]. The result is used read-only (the worktree-list sort), so the writable-source
/// choice is immaterial; a leading UTF-8 **BOM** is still rejected by the parser, a tracked gitana-config
/// gap. `extensions.worktreeConfig` itself is read from the *unexpanded* local file, as git reads it.
pub async fn effective_config_for_worktree(common: &Path, git_dir: &Path) -> Result<GitConfig> {
	let gitdir = canonical_gitdir(git_dir).await;
	let branch = head_branch(git_dir).await;
	let local = local_layer(common).await?;
	// `extensions.worktreeConfig` is a repository-format extension: git honours it **only** from the
	// repository-local config, ignoring any global/system setting, and reads it from the file directly
	// (before includes). A bad boolean aborts, as git does.
	let worktree_config = local
		.source
		.get_bool("extensions", None, "worktreeconfig")?
		.unwrap_or(false);
	let mut base = global_and_system_layers().await?;
	base.push(local);
	if worktree_config
		&& let Some(worktree_layer) = read_layer(&git_dir.join("config.worktree")).await?
	{
		base.push(worktree_layer);
	}
	let gitdir_absolute = logical_gitdir(&gitdir).await;
	assemble_merged(
		base,
		Some(&gitdir),
		gitdir_absolute.as_deref(),
		branch.as_deref(),
	)
	.await
}

/// Read a single config file that must exist — for an explicit `--global`/`--system` `--list`, which
/// git fatals ("unable to read config file") on when the named scope's file is absent. A *keyed*
/// scoped read, by contrast, treats a missing file as empty (see [`read_file`]).
pub async fn read_required(path: &Path) -> Result<GitConfig> {
	let bytes = tokio::fs::read(path)
		.await
		.map_err(|error| anyhow!("unable to read config file '{}': {error}", path.display()))?;
	let text = std::str::from_utf8(&bytes).map_err(|_| anyhow!("{} is not UTF-8", path.display()))?;
	Ok(GitConfig::parse(text)?)
}

/// Reject a malformed `$GIT_CONFIG_NOSYSTEM`. git validates it for **every** config operation, so
/// every path calls this up front.
pub fn ensure_nosystem_valid() -> Result<()> {
	env_bool("GIT_CONFIG_NOSYSTEM").map(|_| ())
}

/// Reject a malformed `$GIT_CONFIG_COUNT` (or an incomplete key/value pair). git parses command-line
/// config for every operation **except** a directly-scoped `--global`/`--system` *keyed* read, which
/// it answers from the one file without consulting command-line config — so that path skips this.
pub fn ensure_count_valid() -> Result<()> {
	env_config_source().map(|_| ())
}

/// The ambient effective config — the global and system layers with no repository — for an unscoped
/// read run outside a repository. Stock `git config <key>` resolves from this stack when there is no
/// repo; only an unscoped *write* requires one. `[include]`/`includeIf` directives are expanded; with
/// no repository, `gitdir:`/`onbranch:` conditions never match, but `hasconfig:remote.*.url:` can still
/// match a global/system remote URL.
pub async fn ambient_effective() -> Result<GitConfig> {
	assemble_merged(global_and_system_layers().await?, None, None, None).await
}

/// The single file an explicit `gta config --global` / `--system` operation reads and writes (git
/// scopes to one file, unlike the merged read). `Global` is [`global_scope_path`]; `System` is
/// [`system_path`].
pub fn write_path(scope: ConfigScope) -> Result<PathBuf> {
	match scope {
		ConfigScope::Local => Err(anyhow!("local config is written through the repository")),
		ConfigScope::Global => global_scope_path(),
		ConfigScope::System => Ok(system_path()),
	}
}

/// Read a single config file into a one-source [`GitConfig`] (writable), or an empty writable config
/// if the file is absent. Used to read-modify-write a scope's file.
pub async fn read_file(path: &Path) -> Result<GitConfig> {
	match tokio::fs::read(path).await {
		Ok(bytes) => {
			let text =
				String::from_utf8(bytes).map_err(|_| anyhow!("{} is not UTF-8", path.display()))?;
			Ok(GitConfig::parse(&text)?)
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(GitConfig::new()),
		Err(error) => Err(anyhow!("reading {}: {error}", path.display())),
	}
}

/// Write a config file's rendered text with git's semantics: atomically, through an exclusive
/// `<path>.lock` file that is renamed into place, and **without** creating missing parent directories.
/// So a write to `/dev/null` (a common way to disable a config), or below a nonexistent directory,
/// fails to acquire the lock — exactly as git does — rather than silently discarding or creating it.
pub async fn write_file(path: &Path, config: &GitConfig) -> Result<()> {
	// Follow a symlinked config file to its real target, so the write updates the target and leaves the
	// symlink in place (a dotfile-managed `~/.gitconfig` stays a link), exactly as git does.
	let target = resolve_symlink(path).await;

	let mut lock = target.as_os_str().to_owned();
	lock.push(".lock");
	let lock = PathBuf::from(lock);

	// `create_new` fails if the lock already exists (a concurrent writer) or its directory is absent
	// (`/dev/null.lock`, a missing parent), matching git's "could not lock config file".
	let mut file = tokio::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&lock)
		.await
		.map_err(|error| anyhow!("could not lock config file '{}': {error}", target.display()))?;

	// Preserve the existing file's permission mode (e.g. a private `0600`) rather than resetting it to
	// the process umask — git copies the mode onto the new file.
	if let Ok(metadata) = tokio::fs::metadata(&target).await {
		let _ = tokio::fs::set_permissions(&lock, metadata.permissions()).await;
	}

	let bytes = config.render().into_bytes();
	let result = async {
		file.write_all(&bytes).await?;
		file.sync_all().await?;
		// A successful rename consumes the lock; a failed one (e.g. the target is a directory) leaves it.
		tokio::fs::rename(&lock, &target).await
	}
	.await;
	if let Err(error) = result {
		// Clean up the lock on any failure — write, sync, or rename — so no stale `<path>.lock` remains.
		let _ = tokio::fs::remove_file(&lock).await;
		return Err(anyhow!("writing {}: {error}", target.display()));
	}
	Ok(())
}

/// Resolve a symlinked config path to its real target so a write updates that target and preserves
/// the link — as git does. The chain is followed by hand (not `canonicalize`, which requires the
/// target to exist) so a link to a **not-yet-existing** target still resolves to that target: the
/// write then creates it and leaves the symlink in place. A non-symlink is returned unchanged.
async fn resolve_symlink(path: &Path) -> PathBuf {
	let mut current = path.to_path_buf();
	// Bound the walk like git's symlink-resolution depth, so a cyclic link cannot loop forever.
	for _ in 0..40 {
		match tokio::fs::symlink_metadata(&current).await {
			Ok(metadata) if metadata.file_type().is_symlink() => {
				match tokio::fs::read_link(&current).await {
					// A relative link target resolves against the link's own directory.
					Ok(target) if target.is_absolute() => current = target,
					Ok(target) => current = current.parent().unwrap_or(Path::new("")).join(target),
					Err(_) => return current,
				}
			}
			// Not a symlink (or the target does not exist yet): this is the file to write.
			_ => return current,
		}
	}
	current
}

/// One config file as an expandable layer: its parsed source plus the two directories git resolves its
/// includes against — the **lexical** parent (the path it was reached through, for a relative
/// `include.path`) and the **real** symlink-resolved parent (for a `gitdir:./` condition). They differ
/// only when the file is reached via a symlink; git treats the two cases differently, so both are kept.
///
/// `fileless` marks the command-scope (`-c` / `GIT_CONFIG_*`) layer, which has no containing file: its
/// includes are expanded through the engine's command-scope entry points (a relative `include.path` is
/// then fatal and a `gitdir:./` condition never matches, as in git), and `dir`/`real_dir` are unused.
struct ConfigLayer {
	source: GitConfigSource,
	dir: PathBuf,
	real_dir: PathBuf,
	fileless: bool,
}

/// Drop the per-layer directories once expansion is done, yielding the sources in read order.
fn sources_of(layers: Vec<ConfigLayer>) -> Vec<GitConfigSource> {
	layers.into_iter().map(|layer| layer.source).collect()
}

/// Assemble a merged [`GitConfig`] from ordered `base` layers (lowest → highest precedence, the last
/// being the writable local file) plus the command-scope (`-c` / `GIT_CONFIG_*`) entries — expanding
/// `[include]`/`includeIf` across **all** of them, command-scope included. git reads `-c` config as
/// part of the same sequence, so a `-c include.path=<abs>` is expanded and a `-c remote.<n>.url` feeds
/// a file-level `hasconfig` condition and the paradox pre-scan; this threads the command-scope source
/// through `expand_layers` for that, then overlays it so it wins on reads while writes still target the
/// writable base. `gitdir`/`branch` drive `gitdir:`/`onbranch:` (`None` outside a repository).
async fn assemble_merged(
	mut base: Vec<ConfigLayer>,
	gitdir: Option<&Path>,
	gitdir_absolute: Option<&Path>,
	branch: Option<&str>,
) -> Result<GitConfig> {
	let base_len = base.len();
	if let Some(env) = env_config_source()? {
		// Command-scope config has no containing file. It is expanded through the engine's command-scope
		// entry points (see `expand_layers`), so — matching git — an absolute `-c include.path` expands,
		// a relative one is fatal, and a `gitdir:./` condition never matches.
		base.push(ConfigLayer {
			source: env,
			dir: PathBuf::new(),
			real_dir: PathBuf::new(),
			fileless: true,
		});
	}
	expand_layers(&mut base, gitdir, gitdir_absolute, branch).await?;
	let mut sources = sources_of(base);
	// Peel the command-scope source(s) back off the top so they overlay (highest precedence for reads)
	// while `from_sources_or_empty` keeps the writable source on the base's last layer (the local file).
	let env_sources = sources.split_off(base_len);
	let mut config = GitConfig::from_sources_or_empty(sources);
	config.overlay(env_sources);
	Ok(config)
}

/// The system and global layers beneath the repository config, in read order (lowest precedence
/// first): system, then the global file(s). The system layer is suppressed here — the merged read —
/// when `$GIT_CONFIG_NOSYSTEM` is set; an explicit `--system` scope still targets the file (see
/// [`system_path`]), matching git.
async fn global_and_system_layers() -> Result<Vec<ConfigLayer>> {
	let mut paths = Vec::new();
	if !env_bool("GIT_CONFIG_NOSYSTEM")? {
		paths.push(system_path());
	}
	paths.extend(global_paths());
	let mut layers = Vec::new();
	for path in paths {
		if let Some(layer) = read_layer(&path).await? {
			layers.push(layer);
		}
	}
	Ok(layers)
}

/// The repository-local layer (`<common>/config`). An absent file yields an empty writable source
/// anchored at `common` — matching git, which resolves from the ambient layers when `.git/config` does
/// not exist yet. A present-but-malformed file is an error (via [`read_layer`]).
async fn local_layer(common: &Path) -> Result<ConfigLayer> {
	Ok(
		read_layer(&common.join("config"))
			.await?
			.unwrap_or_else(|| ConfigLayer {
				source: GitConfigSource::new(),
				dir: common.to_path_buf(),
				real_dir: common.to_path_buf(),
				fileless: false,
			}),
	)
}

/// Parse one config file into a [`ConfigLayer`], or `None` if it is absent. A file that exists but
/// cannot be read or parsed is an error — git aborts on a bad config, so a lower-precedence one must
/// not be silently ignored.
async fn read_layer(path: &Path) -> Result<Option<ConfigLayer>> {
	match tokio::fs::read(path).await {
		Ok(bytes) => {
			let text =
				std::str::from_utf8(&bytes).map_err(|_| anyhow!("{} is not UTF-8", path.display()))?;
			let source = GitConfigSource::parse(text)
				.map_err(|error| anyhow!("parsing {}: {error}", path.display()))?;
			Ok(Some(ConfigLayer {
				source,
				// Lexical parent — the path git reached the file through — for relative `include.path`.
				dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
				// Real (symlink-resolved) parent for `gitdir:./`, as git realpaths the config file.
				real_dir: canonical_dir(path).await,
				fileless: false,
			}))
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(anyhow!("reading {}: {error}", path.display())),
	}
}

/// The real (symlink-resolved) parent directory of a config file, for `gitdir:./` matching — git
/// realpaths the config file before taking its directory. Falls back to the lexical parent if
/// canonicalization fails.
async fn canonical_dir(path: &Path) -> PathBuf {
	let canonical = tokio::fs::canonicalize(path)
		.await
		.unwrap_or_else(|_| path.to_path_buf());
	canonical
		.parent()
		.map(Path::to_path_buf)
		.unwrap_or_default()
}

/// The real (symlink-resolved, absolute) git directory for `includeIf "gitdir:"`/`"onbranch:"`
/// matching. git realpaths `$GIT_DIR`; discovery already canonicalizes, but resolve again defensively.
async fn canonical_gitdir(git_dir: &Path) -> PathBuf {
	tokio::fs::canonicalize(git_dir)
		.await
		.unwrap_or_else(|_| git_dir.to_path_buf())
}

/// git's **second** `gitdir:`-condition candidate: the git directory spelled through the
/// symlink-preserving `$PWD` git honours (its `strbuf_add_absolute_path(git_dir)` fallback), or `None`
/// when there is no distinct spelling. git matches a `gitdir:` condition against `realpath(git_dir)`
/// *and* this, so a condition written with a symlinked path still matches a repository reached through
/// that symlink.
///
/// git honours `$PWD` as the logical working directory only when it resolves to the real one, and it
/// carries the symlink spelling into `opts->git_dir` only for a repository entered **at its root** —
/// where git's relative `opts->git_dir` is `".git"` (ordinary) or `"."` (bare). Its
/// `strbuf_add_absolute_path` fallback is then that relative name joined onto the honoured cwd. From a
/// subdirectory git `chdir`s up during discovery and records the realpath, so no distinct spelling
/// exists (probed vs git 2.50.1: a symlink-spelled `gitdir:` condition matches from the repo root — and
/// for a bare root, e.g. `gitdir:/link.git/` — but not from a subdirectory).
///
/// This reproduces that: `$PWD` must be set, absolute, carry a symlink of its own, and canonicalize to
/// the command's working directory (the `-C` dir, else the process cwd). The candidate is `$PWD` joined
/// with the gitdir *relative to that cwd* — `".git"` for an ordinary root, `"."` for a bare root (its
/// gitdir *is* the cwd; git's unnormalised `getcwd + "/."`), and no relation (so `None`) from a
/// subdirectory, a linked worktree, or a symlinked `.git`, where the spelling is not `$PWD`-derived. The
/// final `canonicalize` check means any mis-derivation degrades to canonical-only — never a spurious
/// match.
async fn logical_gitdir(canonical_git_dir: &Path) -> Option<PathBuf> {
	let pwd = PathBuf::from(std::env::var_os("PWD")?);
	if !pwd.is_absolute() {
		return None;
	}
	let effective_cwd = command_cwd().or_else(|| std::env::current_dir().ok())?;
	let cwd_real = tokio::fs::canonicalize(&effective_cwd).await.ok()?;
	let pwd_real = tokio::fs::canonicalize(&pwd).await.ok()?;
	// `$PWD` must name the real working directory (git's honouring rule) and carry a symlink of its own.
	if pwd_real != cwd_real || pwd == pwd_real {
		return None;
	}
	// The gitdir relative to the honoured cwd, matching git's relative `opts->git_dir`: `.git` for an
	// ordinary root, empty (→ `.`, git's `getcwd + "/."`) for a bare root, and unrelated (→ `None`) from
	// a subdirectory or a gitdir not under the cwd (linked worktree / symlinked `.git`).
	let relative = canonical_git_dir.strip_prefix(&cwd_real).ok()?;
	let candidate = if relative.as_os_str().is_empty() {
		pwd.join(".")
	} else {
		pwd.join(relative)
	};
	// Safety net: offer it only if it is genuinely a distinct symlink spelling of the same gitdir.
	if candidate == canonical_git_dir
		|| tokio::fs::canonicalize(&candidate).await.ok()? != canonical_git_dir
	{
		return None;
	}
	Some(candidate)
}

/// The short current-branch name for `includeIf "onbranch:"` — the target of a **symbolic** `HEAD`
/// under `refs/heads/`, with the prefix stripped (so it is present for an unborn branch and a bare
/// repo, and `None` only for a detached HEAD or a HEAD outside `refs/heads/`, matching git).
async fn head_branch(git_dir: &Path) -> Option<String> {
	let head = tokio::fs::read_to_string(git_dir.join("HEAD")).await.ok()?;
	let target = head.strip_prefix("ref:")?.trim();
	target
		.strip_prefix("refs/heads/")
		.map(std::borrow::ToOwned::to_owned)
}

/// A [`tokio::fs`]-backed [`IncludeResolver`], the native driver for git-config include expansion. An
/// absent target reads as `None` (git silently skips it); a present-but-unreadable or non-UTF-8 target
/// is an error, as git aborts on a bad included file.
struct FsIncludeResolver;

impl IncludeResolver for FsIncludeResolver {
	async fn read(&self, path: &Path) -> Result<Option<(String, PathBuf)>, ConfigError> {
		match tokio::fs::read(path).await {
			Ok(bytes) => {
				let text = String::from_utf8(bytes)
					.map_err(|_| ConfigError::Parse(format!("{} is not UTF-8", path.display())))?;
				// Report the file's real path so the engine matches a nested `gitdir:./` against its real
				// directory (git realpaths each included file for that condition). If canonicalization fails
				// — it should not, the file was just read — fall back to the requested path.
				let canonical = tokio::fs::canonicalize(path)
					.await
					.unwrap_or_else(|_| path.to_path_buf());
				Ok(Some((text, canonical)))
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
			Err(error) => Err(ConfigError::Parse(format!(
				"reading include {}: {error}",
				path.display()
			))),
		}
	}
}

/// Expand `[include]`/`includeIf` across all `layers` in place, reproducing git's whole-config
/// handling. First runs git's remote-URL pre-scan ([`GitConfigSource::scan_remote_urls`]) over every
/// layer, combining the results: the collected `remote.<name>.url` values feed `hasconfig` matching,
/// and if any layer reached a `hasconfig` directive **and** any layer's matched-`includeIf` subtree set
/// a remote URL, that is git's paradox — fatal across the whole config. Then expands each layer with the
/// collected URLs so `hasconfig` resolves against every layer, as git does. `gitdir`/`branch` are the
/// repository facts for `gitdir:`/`onbranch:` (`None` outside a repository).
async fn expand_layers(
	layers: &mut [ConfigLayer],
	gitdir: Option<&Path>,
	gitdir_absolute: Option<&Path>,
	branch: Option<&str>,
) -> Result<()> {
	let home = home_dir();
	let resolver = FsIncludeResolver;
	// Pre-scan: hasconfig is forced true inside the scan, so it consults no remote URLs itself.
	let prescan_ctx = IncludeContext {
		home: home.as_deref(),
		gitdir,
		gitdir_absolute,
		branch,
		remote_urls: None,
	};
	let mut urls: Vec<String> = Vec::new();
	let mut has_hasconfig = false;
	let mut forbidden_url = false;
	for layer in layers.iter() {
		let scan = if layer.fileless {
			layer
				.source
				.scan_remote_urls_command_scope(&prescan_ctx, &resolver)
				.await
		} else {
			layer
				.source
				.scan_remote_urls(&layer.dir, &layer.real_dir, &prescan_ctx, &resolver)
				.await
		}
		.map_err(|error| anyhow!("reading git config includes: {error}"))?;
		urls.extend(scan.urls);
		has_hasconfig |= scan.has_hasconfig;
		forbidden_url |= scan.forbidden_url;
	}
	// git fatals on the paradox only when a `hasconfig` directive exists to trigger the pre-scan.
	if has_hasconfig && forbidden_url {
		return Err(anyhow!("{}", ConfigError::HasconfigIncludeSetsRemoteUrl));
	}
	let url_refs: Vec<&str> = urls.iter().map(String::as_str).collect();
	let ctx = IncludeContext {
		home: home.as_deref(),
		gitdir,
		gitdir_absolute,
		branch,
		remote_urls: Some(url_refs.as_slice()),
	};
	for layer in layers.iter_mut() {
		if layer.fileless {
			layer
				.source
				.expand_includes_command_scope(&ctx, &resolver)
				.await
		} else {
			layer
				.source
				.expand_includes(&layer.dir, &layer.real_dir, &ctx, &resolver)
				.await
		}
		.map_err(|error| anyhow!("expanding git config includes: {error}"))?;
	}
	Ok(())
}

/// Config entries passed through the environment: `GIT_CONFIG_COUNT` with `GIT_CONFIG_KEY_<n>` /
/// `GIT_CONFIG_VALUE_<n>` pairs — git's mechanism for propagating `-c key=value` options. They sit at
/// the very top of the precedence stack (above the repository-local file) for a *merged* read only; an
/// explicitly scoped `--global`/`--system`/`--local` lookup ignores them, as git does. `None` when
/// `GIT_CONFIG_COUNT` is unset or zero; an error (as git aborts) on a malformed count or a missing
/// key/value pair.
fn env_config_source() -> Result<Option<GitConfigSource>> {
	let count = match std::env::var("GIT_CONFIG_COUNT") {
		// Only the exact empty string is "unset"; any other value (including whitespace like `" 1 "` or
		// `"   "`) is parsed strictly, so git's "bogus count" surfaces rather than being trimmed away.
		Ok(value) if value.is_empty() => return Ok(None),
		Ok(value) => value
			.parse::<usize>()
			.map_err(|_| anyhow!("bogus count in GIT_CONFIG_COUNT: {value}"))?,
		Err(_) => return Ok(None),
	};
	if count == 0 {
		return Ok(None);
	}
	let mut source = GitConfigSource::new();
	for n in 0..count {
		let key = std::env::var(format!("GIT_CONFIG_KEY_{n}"))
			.map_err(|_| anyhow!("missing GIT_CONFIG_KEY_{n}"))?;
		let value = std::env::var(format!("GIT_CONFIG_VALUE_{n}"))
			.map_err(|_| anyhow!("missing GIT_CONFIG_VALUE_{n}"))?;
		let (section, subsection, name) = parse_env_key(&key)?;
		// `append` (not `add`) preserves strict command-line order: each entry becomes its own block, so
		// an `include.path` interleaved with repeats of a key expands at its true position and git's
		// last-entry-wins holds. `add` would group a repeat back into an earlier section, moving it past
		// the include. Multi-valued accumulation and single-value last-wins are preserved either way.
		source.append(section, subsection, name, Some(&value));
	}
	Ok(Some(source))
}

/// Split a dotted `GIT_CONFIG_KEY_<n>` into `(section, subsection, name)`: the first `.` ends the
/// section, the last `.` begins the name, and anything between is the (case-sensitive) subsection.
/// Validates git's key grammar — the section is alphanumeric/`-`, the variable name starts with a
/// letter and is otherwise alphanumeric/`-`, and the subsection is freeform — since git rejects a
/// malformed propagated `-c` key (e.g. `user.na_me`, `a.1`) before running.
fn parse_env_key(key: &str) -> Result<(&str, Option<&str>, &str)> {
	let first = key
		.find('.')
		.ok_or_else(|| anyhow!("invalid config key '{key}' (no section)"))?;
	let last = key.rfind('.').unwrap();
	let section = &key[..first];
	let name = &key[last + 1..];
	let subsection = (first != last).then(|| &key[first + 1..last]);

	let section_ok = !section.is_empty()
		&& section
			.bytes()
			.all(|c| c.is_ascii_alphanumeric() || c == b'-');
	let name_ok = {
		let mut bytes = name.bytes();
		matches!(bytes.next(), Some(c) if c.is_ascii_alphabetic())
			&& bytes.all(|c| c.is_ascii_alphanumeric() || c == b'-')
	};
	if !section_ok || !name_ok {
		return Err(anyhow!("invalid config key '{key}'"));
	}
	Ok((section, subsection, name))
}

/// The system config file: `$GIT_CONFIG_SYSTEM` or the built-in `/etc/gitconfig`. This is
/// unconditional — `$GIT_CONFIG_NOSYSTEM` only drops the system layer from the *merged* read
/// ([`global_and_system_layers`]); an explicitly named `--system` scope still reads/writes it, as
/// git does.
fn system_path() -> PathBuf {
	env_override_path("GIT_CONFIG_SYSTEM").unwrap_or_else(|| PathBuf::from("/etc/gitconfig"))
}

/// The global config file paths, in read order (lowest precedence first). `$GIT_CONFIG_GLOBAL`
/// (e.g. `/dev/null` to disable) names exactly one file — even when *empty*, which git honours as "no
/// global config" rather than falling back; otherwise git reads the XDG file
/// (`$XDG_CONFIG_HOME/git/config` or `~/.config/git/config`) then `~/.gitconfig`, the latter winning.
fn global_paths() -> Vec<PathBuf> {
	if let Some(path) = env_override_path("GIT_CONFIG_GLOBAL") {
		return vec![path];
	}
	let mut paths = Vec::new();
	if let Some(xdg) = xdg_config_home() {
		paths.push(xdg.join("git").join("config"));
	}
	if let Some(home) = home_dir() {
		paths.push(home.join(".gitconfig"));
	}
	paths
}

/// The single file the explicit `--global` scope reads and writes: `$GIT_CONFIG_GLOBAL` if set (even
/// empty — an empty override is "no global config", so a read finds nothing and a write fails, as
/// git does); else an existing `~/.gitconfig`, then an existing XDG file, else `~/.gitconfig`
/// (created on write, read as empty when absent). Mirrors git, which resolves `--global` to
/// `~/.gitconfig` unless only the XDG file exists — distinct from the merged read (see [`global_paths`]).
fn global_scope_path() -> Result<PathBuf> {
	if let Some(path) = env_override_path("GIT_CONFIG_GLOBAL") {
		return Ok(path);
	}
	// An explicit `--global` requires HOME, even when an XDG file exists — git fatals with
	// "$HOME not set" rather than falling back to the XDG path for a named-scope operation.
	let home = home_dir()
		.ok_or_else(|| anyhow!("$HOME not set"))?
		.join(".gitconfig");
	if home.exists() {
		return Ok(home);
	}
	if let Some(xdg) = xdg_config_home().map(|x| x.join("git").join("config"))
		&& xdg.exists()
	{
		return Ok(xdg);
	}
	Ok(home)
}

/// `$XDG_CONFIG_HOME`, or `~/.config` when it is unset or empty.
fn xdg_config_home() -> Option<PathBuf> {
	env_path("XDG_CONFIG_HOME").or_else(|| home_dir().map(|h| h.join(".config")))
}

/// The user's home directory (`$HOME`).
fn home_dir() -> Option<PathBuf> {
	env_path("HOME")
}

/// A non-empty environment path, or `None` — for discovery inputs (`$HOME`, `$XDG_CONFIG_HOME`) where
/// git treats an empty value as unset and falls back. A relative value resolves against the command's
/// working directory (git resolves these config roots after applying `-C`).
fn env_path(name: &str) -> Option<PathBuf> {
	let raw = std::env::var_os(name)?;
	if raw.is_empty() {
		return None;
	}
	Some(resolve_command_relative(PathBuf::from(raw)))
}

/// An explicit config-file override (`$GIT_CONFIG_GLOBAL` / `$GIT_CONFIG_SYSTEM`), honoured even when
/// *empty*: git treats a set-but-empty value as naming a nonexistent file ("no such config"), not as
/// unset, so it never falls back to the discovered default. A relative path is resolved against the
/// command's working directory (see [`COMMAND_CWD`]). `None` only when the variable is absent.
fn env_override_path(name: &str) -> Option<PathBuf> {
	Some(resolve_command_relative(PathBuf::from(std::env::var_os(
		name,
	)?)))
}

/// Resolve `path` against the command's working directory ([`COMMAND_CWD`]) when it is relative, so
/// relative config roots and overrides land under the `-C` directory as git resolves them. An
/// absolute path, and the empty "no such file" override, are returned verbatim.
fn resolve_command_relative(path: PathBuf) -> PathBuf {
	if path.is_absolute() || path.as_os_str().is_empty() {
		return path;
	}
	COMMAND_CWD.try_with(|cwd| cwd.join(&path)).unwrap_or(path)
}

/// A git-style boolean environment variable: `true`/`yes`/`on` (any case) → true;
/// `false`/`no`/`off`/empty → false; any integer → its truthiness (nonzero is true, so `2`/`-1` are
/// true and `0` is false); unset → false. Anything else is an error, as git aborts on a bad boolean
/// environment value. Used for `$GIT_CONFIG_NOSYSTEM`.
fn env_bool(name: &str) -> Result<bool> {
	match std::env::var(name) {
		Err(_) => Ok(false),
		Ok(value) => parse_git_bool(&value)
			.ok_or_else(|| anyhow!("bad boolean environment value '{value}' for '{name}'")),
	}
}

/// Git's boolean grammar for an environment value (see [`env_bool`]); `None` for a value git rejects.
pub(crate) fn parse_git_bool(value: &str) -> Option<bool> {
	match value.to_ascii_lowercase().as_str() {
		"true" | "yes" | "on" => Some(true),
		"" | "false" | "no" | "off" => Some(false),
		// git falls back to integer truthiness: any nonzero integer is true, zero is false.
		other => other.parse::<i64>().ok().map(|n| n != 0),
	}
}
