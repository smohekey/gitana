//! Discovery of git's configuration files and their precedence.
//!
//! The engine's [`Repository::read_config`] reads only the repository-local `.git/config` — it holds
//! a capability scoped to the git directory and cannot reach the user's `~/.gitconfig` or the system
//! `/etc/gitconfig`. Layering those in is a frontend concern (ambient path I/O), so it lives here.
//!
//! [`effective_config`] assembles git's full precedence stack — system, then global (XDG then
//! `~/.gitconfig`, or a single `$GIT_CONFIG_GLOBAL`), then repo-local — into one layered
//! [`GitConfig`]. Reads resolve across every layer (git's last-writer-wins); writes stay directed at
//! the repository-local file. The [`ConfigScope`] helpers resolve the single file `gta config
//! --global` / `--system` read and write, honouring the same `GIT_CONFIG_*` environment git does.

use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use gitana_config::{GitConfig, GitConfigSource};
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;
use tokio::io::AsyncWriteExt;

use crate::Backend;

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

/// The effective configuration: the repository-local config underlaid with the global and system
/// files, in git's precedence order (system < global < local). Reads see every layer; writes and
/// [`GitConfig::render`] stay on the local file. A repo whose local config is unreadable (e.g. a
/// clone resolving its committer before the config exists) still resolves identity from the global
/// and system layers. A global/system file that exists but is malformed or unreadable is an error,
/// as git aborts on a bad config file.
pub async fn effective_config<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
) -> Result<GitConfig> {
	let mut config = repo
		.read_config()
		.await
		.unwrap_or_else(|_| GitConfig::new());
	config.underlay(global_and_system_sources().await?);
	config.overlay(env_config_source()?);
	Ok(config)
}

/// The effective configuration keyed on a git/common directory path rather than an open
/// [`Repository`] — for the remote commands, which resolve credentials (and build the transport)
/// before opening the repo (or, for `fetch`/`push`, before the hash algorithm the repo is generic
/// over is even known). Reads the local `<dir>/config` and underlays global/system, then overlays the
/// `-c`/`GIT_CONFIG_*` command-line entries, exactly like [`effective_config`]. A missing local
/// config resolves from the global/system layers alone.
pub async fn effective_config_at(dir: &Path) -> Result<GitConfig> {
	let mut config = read_file(&dir.join("config")).await?;
	config.underlay(global_and_system_sources().await?);
	config.overlay(env_config_source()?);
	Ok(config)
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
/// repo; only an unscoped *write* requires one.
pub async fn ambient_effective() -> Result<GitConfig> {
	let mut config = GitConfig::from_sources_or_empty(global_and_system_sources().await?);
	config.overlay(env_config_source()?);
	Ok(config)
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

/// The system and global sources beneath the repository config, in read order (lowest precedence
/// first): system, then the global file(s). The system layer is suppressed here — the merged read —
/// when `$GIT_CONFIG_NOSYSTEM` is set; an explicit `--system` scope still targets the file (see
/// [`system_path`]), matching git.
async fn global_and_system_sources() -> Result<Vec<GitConfigSource>> {
	let mut paths = Vec::new();
	if !env_bool("GIT_CONFIG_NOSYSTEM")? {
		paths.push(system_path());
	}
	paths.extend(global_paths());
	read_sources(paths).await
}

/// Parse each file at `paths` into a source, in order. An absent file is skipped; a file that exists
/// but cannot be read or parsed is an error — git aborts on a bad config, so a lower-precedence one
/// must not be silently ignored.
async fn read_sources(paths: Vec<PathBuf>) -> Result<Vec<GitConfigSource>> {
	let mut sources = Vec::new();
	for path in paths {
		match tokio::fs::read(&path).await {
			Ok(bytes) => {
				let text =
					std::str::from_utf8(&bytes).map_err(|_| anyhow!("{} is not UTF-8", path.display()))?;
				let source = GitConfigSource::parse(text)
					.map_err(|error| anyhow!("parsing {}: {error}", path.display()))?;
				sources.push(source);
			}
			// An absent file simply contributes nothing to the stack.
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(anyhow!("reading {}: {error}", path.display())),
		}
	}
	Ok(sources)
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
		// `add` (not `set`) so repeated `-c` of a multi-valued key accumulate, last-wins for a
		// single-valued lookup — matching git's handling of `-c`.
		source.add(section, subsection, name, Some(&value));
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
/// ([`global_and_system_sources`]); an explicitly named `--system` scope still reads/writes it, as
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
