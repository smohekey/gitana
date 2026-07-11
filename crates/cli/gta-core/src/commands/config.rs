use std::path::Path;

use crate::Backend;
use anyhow::{Result, anyhow, bail};
use gitana_config::GitConfig;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::dispatch::{self, RepoCommand};
use crate::git_config::{self, ConfigScope};

/// Read or write git configuration.
///
/// The scope selects the file: `--local` (the default for writes) is the repository `.git/config`;
/// `--global` is the user config (`~/.gitconfig` / XDG); `--system` is `/etc/gitconfig`. A read
/// without a scope resolves across git's whole precedence stack (system → global → local); a write
/// without a scope lands in the local file, as git does.
///
/// With a `name` and `value`, sets the variable; with `name` only, prints its value (exit non-zero
/// if unset). `get_all` prints every value, `add` appends one, `replace_all` overwrites every value
/// of a key with one, `unset` removes the variable, and `list` prints all variables as `key=value`.
/// `as_bool`/`as_int` interpret the read value. Writes are surgical: they edit the affected line in
/// place and leave comments and the surrounding layout untouched.
#[allow(clippy::too_many_arguments)]
pub async fn run(
	cwd: &Path,
	get: bool,
	get_all: bool,
	add: bool,
	replace_all: bool,
	unset: bool,
	list: bool,
	as_bool: bool,
	as_int: bool,
	global: bool,
	system: bool,
	local: bool,
	name: Option<String>,
	value: Option<String>,
) -> Result<()> {
	// git applies `-C <dir>` (a chdir) before anything else, so a missing or non-directory `-C` aborts
	// even for a scope that would not otherwise touch the working directory (`--global`/`--system`, or
	// an unscoped read that falls back to ambient config).
	match tokio::fs::metadata(cwd).await {
		Ok(metadata) if metadata.is_dir() => {}
		Ok(_) => bail!("cannot change to '{}': Not a directory", cwd.display()),
		Err(error) => bail!("cannot change to '{}': {error}", cwd.display()),
	}

	// GIT_CONFIG_NOSYSTEM is validated for every config operation.
	git_config::ensure_nosystem_valid()?;

	let scope = resolve_scope(global, system, local)?;
	let args = ConfigArgs {
		get,
		get_all,
		add,
		replace_all,
		unset,
		list,
		as_bool,
		as_int,
		name,
		value,
	};

	// git parses GIT_CONFIG_COUNT for every operation *except* a directly-scoped `--global`/`--system`
	// single-key read, which it answers from the one file without consulting command-line config. A
	// `--get-all` (like `--list`) is not exempt — git validates the count for it.
	let scoped_keyed_read = matches!(scope, Some(ConfigScope::Global) | Some(ConfigScope::System))
		&& !args.list
		&& !args.get_all
		&& write_op(&args)?.is_none();
	if !scoped_keyed_read {
		git_config::ensure_count_valid()?;
	}

	// `--global` / `--system` never need a repository (git config --global works anywhere); `--local`
	// always does; and the unscoped default reads from the full stack but writes to the repository.
	match scope {
		Some(ConfigScope::Global) => run_ambient(ConfigScope::Global, args).await,
		Some(ConfigScope::System) => run_ambient(ConfigScope::System, args).await,
		Some(ConfigScope::Local) => {
			dispatch::on_repo(
				cwd,
				ConfigCmd {
					local_only: true,
					args,
				},
			)
			.await
		}
		None => run_unscoped(cwd, args).await,
	}
}

/// The unscoped default. A read resolves across the full precedence stack; outside a repository it
/// still resolves from the ambient (global + system) stack, as git does. A write always targets the
/// repository-local file, so it requires a repository.
async fn run_unscoped(cwd: &Path, args: ConfigArgs) -> Result<()> {
	let is_write = !args.list && write_op(&args)?.is_some();
	// A read genuinely outside a repository (`try_discover` → `None`) falls back to the ambient stack;
	// a *malformed* repository is an error (propagated by `?`), not a fall-through — matching git.
	if !is_write && crate::repo::try_discover(cwd).await?.is_none() {
		return emit_reads(&git_config::ambient_effective().await?, &args);
	}
	dispatch::on_repo(
		cwd,
		ConfigCmd {
			local_only: false,
			args,
		},
	)
	.await
}

/// The parsed `gta config` flags shared by the repository and ambient (global/system) paths.
struct ConfigArgs {
	get: bool,
	get_all: bool,
	add: bool,
	replace_all: bool,
	unset: bool,
	list: bool,
	as_bool: bool,
	as_int: bool,
	name: Option<String>,
	value: Option<String>,
}

/// A `gta config` operation over the repository (`--local` or the unscoped default).
struct ConfigCmd {
	/// True for an explicit `--local`: reads see only the local file. The unscoped default reads the
	/// full precedence stack.
	local_only: bool,
	args: ConfigArgs,
}

impl RepoCommand for ConfigCmd {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		let ConfigCmd { local_only, args } = self;

		match read_or_write(&args)? {
			// Writes land in the local file — git's default write scope, whether or not `--local` was
			// explicit.
			Some((op, name, section, subsection, var)) => {
				let mut config = repo.read_config().await?;
				apply_write(&mut config, &op, section, subsection, var, &name)?;
				repo.write_config(&config).await.map_err(Into::into)
			}
			None => emit_reads(&read_repo_config(&repo, local_only).await?, &args),
		}
	}
}

/// A `gta config --global` / `--system` operation: ambient file I/O, no repository. Read and write
/// both target the scope's single file. A missing file matches git's asymmetry: a `--list` fatals,
/// but a keyed read (or a write) treats it as empty.
async fn run_ambient(scope: ConfigScope, args: ConfigArgs) -> Result<()> {
	let path = git_config::write_path(scope)?;
	match read_or_write(&args)? {
		Some((op, name, section, subsection, var)) => {
			let mut config = git_config::read_file(&path).await?;
			apply_write(&mut config, &op, section, subsection, var, &name)?;
			git_config::write_file(&path, &config).await
		}
		// `--list` requires the file to exist; a keyed read treats a missing file as unset.
		None if args.list => {
			emit_list(&git_config::read_required(&path).await?);
			Ok(())
		}
		None => emit_reads(&git_config::read_file(&path).await?, &args),
	}
}

/// Classify the operation: a write (with its resolved op and parsed key) or a read (`None`). `--list`
/// is always a read; a key is required for any non-list operation.
#[allow(clippy::type_complexity)]
fn read_or_write(args: &ConfigArgs) -> Result<Option<(WriteOp, String, &str, Option<&str>, &str)>> {
	if args.list {
		return Ok(None);
	}
	let name = args
		.name
		.as_deref()
		.ok_or_else(|| anyhow!("a config key is required"))?;
	let (section, subsection, var) = parse_key(name)?;
	match write_op(args)? {
		Some(op) => Ok(Some((op, name.to_owned(), section, subsection, var))),
		None => Ok(None),
	}
}

/// Emit a read result: the full listing for `--list`, else the single key's value.
fn emit_reads(config: &GitConfig, args: &ConfigArgs) -> Result<()> {
	if args.list {
		emit_list(config);
		return Ok(());
	}
	let name = args
		.name
		.as_deref()
		.ok_or_else(|| anyhow!("a config key is required"))?;
	let (section, subsection, var) = parse_key(name)?;
	emit_read(config, name, args, section, subsection, var)
}

/// The repository config for a read: the local file alone when `--local` was given, else the full
/// precedence stack (system → global → local).
async fn read_repo_config<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	local_only: bool,
) -> Result<GitConfig> {
	if local_only {
		Ok(repo.read_config().await?)
	} else {
		git_config::effective_config(repo).await
	}
}

/// A pending write, resolved from the flags and value.
enum WriteOp {
	Set(String),
	Add(String),
	ReplaceAll(String),
	Unset,
}

/// Resolve the flags into a pending write, or `None` for a read. Rejects the flag/value combinations
/// git also rejects.
fn write_op(args: &ConfigArgs) -> Result<Option<WriteOp>> {
	if args.unset {
		// Git treats a value with --unset as a value-pattern to match; we do not implement that yet,
		// so reject it rather than deleting the key regardless of its value.
		if args.value.is_some() {
			bail!("--unset does not take a value (value-pattern matching is not supported)");
		}
		return Ok(Some(WriteOp::Unset));
	}
	if args.add {
		let value = value_of(args, "--add requires a value")?;
		return Ok(Some(WriteOp::Add(value)));
	}
	if args.replace_all {
		// Git treats a trailing value with --replace-all as a value-pattern to match; we do not
		// implement that, so a bare value simply overwrites every value.
		let value = value_of(args, "--replace-all requires a value")?;
		return Ok(Some(WriteOp::ReplaceAll(value)));
	}
	// A value (without a read flag) means set.
	if let Some(value) = args.value.clone() {
		if args.get || args.get_all {
			bail!("a get option cannot take a value");
		}
		return Ok(Some(WriteOp::Set(value)));
	}
	Ok(None)
}

fn value_of(args: &ConfigArgs, missing: &str) -> Result<String> {
	args.value.clone().ok_or_else(|| anyhow!("{missing}"))
}

/// Apply a pending write to `config` (the writable source). `unset` fails when the key is absent.
fn apply_write(
	config: &mut GitConfig,
	op: &WriteOp,
	section: &str,
	subsection: Option<&str>,
	var: &str,
	name: &str,
) -> Result<()> {
	match op {
		// Refuses (leaving the file unchanged) if the key already holds multiple values.
		WriteOp::Set(value) => config.set(section, subsection, var, value)?,
		WriteOp::Add(value) => config.add(section, subsection, var, Some(value)),
		WriteOp::ReplaceAll(value) => config.replace_all(section, subsection, var, value),
		WriteOp::Unset => {
			if !config.unset(section, subsection, var) {
				bail!("key '{name}' is not set");
			}
		}
	}
	Ok(())
}

/// Print the read result for a single key, matching git: `--get-all` prints every value; a bare
/// (valueless) variable prints an empty line and succeeds; an absent key exits non-zero.
fn emit_read(
	config: &GitConfig,
	name: &str,
	args: &ConfigArgs,
	section: &str,
	subsection: Option<&str>,
	var: &str,
) -> Result<()> {
	if args.get_all {
		let values = config.get_all_raw(section, subsection, var);
		if values.is_empty() {
			bail!("key '{name}' is not set");
		}
		for v in values {
			println!("{}", v.unwrap_or(""));
		}
		return Ok(());
	}

	let found = if args.as_bool {
		config
			.get_bool(section, subsection, var)?
			.map(|b| b.to_string())
	} else if args.as_int {
		config
			.get_int(section, subsection, var)?
			.map(|n| n.to_string())
	} else {
		config
			.get_raw(section, subsection, var)
			.map(|value| value.unwrap_or("").to_owned())
	};
	match found {
		Some(rendered) => println!("{rendered}"),
		None => bail!("key '{name}' is not set"),
	}
	Ok(())
}

/// Print every variable as `key=value` (a bare variable as just `key`).
fn emit_list(config: &GitConfig) {
	for (key, val) in config.entries() {
		match val {
			Some(v) => println!("{key}={v}"),
			None => println!("{key}"),
		}
	}
}

/// Resolve the mutually exclusive scope flags. At most one may be given; none means the default
/// (merged read / local write).
fn resolve_scope(global: bool, system: bool, local: bool) -> Result<Option<ConfigScope>> {
	match (global, system, local) {
		(false, false, false) => Ok(None),
		(true, false, false) => Ok(Some(ConfigScope::Global)),
		(false, true, false) => Ok(Some(ConfigScope::System)),
		(false, false, true) => Ok(Some(ConfigScope::Local)),
		_ => bail!("only one of --local, --global, --system may be given"),
	}
}

/// Split a dotted config key into `(section, subsection, name)`: the first `.` ends the section,
/// the last `.` begins the name, and anything between is the (case-sensitive) subsection.
fn parse_key(key: &str) -> Result<(&str, Option<&str>, &str)> {
	let first = key
		.find('.')
		.ok_or_else(|| anyhow!("key '{key}' does not contain a section"))?;
	let last = key.rfind('.').unwrap();
	let section = &key[..first];
	let name = &key[last + 1..];
	if section.is_empty() || name.is_empty() {
		bail!("invalid config key: '{key}'");
	}
	let subsection = (first != last).then(|| &key[first + 1..last]);
	Ok((section, subsection, name))
}
