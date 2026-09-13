//! The CLI side of commit/tag/push signing: resolve a signing key and shell out to the signing
//! program, returning the bare armor block the [`Signer`] contract asks for. Two formats, chosen by
//! git config `gpg.format` (`ssh` → [`CliSigner`] over `ssh-keygen -Y sign`; `openpgp` or unset →
//! [`GpgSigner`] over `gpg --detach-sign`, matching git's default). Each program is overridable via the
//! same config git uses — `gpg.ssh.program` for SSH and `gpg.openpgp.program` (or the legacy
//! `gpg.program`) for OpenPGP — so gitana runs whatever binary the repo is already configured for.
//! Subprocesses are awaited through `tokio::process` so they never block the runtime (see
//! `docs/conventions.md`).

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use cap_std::fs::{Dir, File};
use gitana_file_store::FileStore;
use gitana_fs_native::{
	ProcessCurrentDirGuard, ProcessFileGuard, configure_process_current_dir, configure_process_file,
	file_identity, open_process_file,
};
use gitana_object::HashAlgorithm;
use gitana_porcelain::Signer;
use gitana_repository::Repository;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// The default program for each signing format, overridable by the matching git config key.
const DEFAULT_SSH_PROGRAM: &str = "ssh-keygen";
const DEFAULT_GPG_PROGRAM: &str = "gpg";

/// Feed `payload` to `command` on stdin and return its stdout, trimmed of the trailing newline
/// signing programs print (the [`Signer`] contract is a bare armor block, which the object encoder
/// folds into the `gpgsig` header). `what` names the program for error context.
async fn run_signer(
	command: StdCommand,
	payload: &[u8],
	what: &str,
	cwd: &Path,
	cwd_directory: &Dir,
	file_guards: Vec<ProcessFileGuard>,
) -> Result<String> {
	let (mut command, cwd_guard) = configure_signer_current_dir(command, cwd, cwd_directory)
		.await
		.with_context(|| format!("retaining signer working directory {}", cwd.display()))?;
	command.kill_on_drop(true);
	let mut child = command
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.with_context(|| format!("spawning `{what}` (is it installed?)"))?;
	drop(cwd_guard);

	// Feed the payload on stdin; closing it (drop) signals EOF so the program finishes.
	let mut stdin = child.stdin.take().expect("stdin was piped");
	stdin
		.write_all(payload)
		.await
		.with_context(|| format!("writing payload to `{what}`"))?;
	drop(stdin);

	let output = child
		.wait_with_output()
		.await
		.with_context(|| format!("waiting for `{what}`"))?;
	drop(file_guards);
	if !output.status.success() {
		bail!(
			"`{what}` failed: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
	}
	Ok(
		String::from_utf8(output.stdout)
			.with_context(|| format!("`{what}` signature output was not UTF-8"))?
			.trim_end()
			.to_owned(),
	)
}

/// Anchor a signer command to its operation's retained effective working directory so replacement
/// of the public namespace cannot redirect a relative key or program between validation and
/// process creation.
async fn configure_signer_current_dir(
	mut command: StdCommand,
	cwd: &Path,
	cwd_directory: &Dir,
) -> Result<(Command, ProcessCurrentDirGuard)> {
	let directory = cwd_directory
		.try_clone()
		.context("retaining signer working directory")?;
	let cwd = cwd.to_path_buf();
	let (command, guard) = tokio::task::spawn_blocking(move || {
		let guard = configure_process_current_dir(&mut command, &directory, &cwd)?;
		Ok::<_, std::io::Error>((command, guard))
	})
	.await
	.context("joining signer working-directory worker")??;
	Ok((Command::from(command), guard))
}

/// Configure a child-facing retained file without performing filesystem work on the async runtime.
async fn configure_retained_process_file(
	mut command: StdCommand,
	file: Arc<File>,
	display_path: PathBuf,
) -> Result<(StdCommand, PathBuf, ProcessFileGuard)> {
	let retained = tokio::task::spawn_blocking(move || {
		let (child_path, guard) = configure_process_file(&mut command, &file, &display_path)?;
		Ok::<_, std::io::Error>((command, child_path, guard))
	})
	.await
	.context("joining signer resource-retention worker")??;
	Ok(retained)
}

/// A [`Signer`] backed by a local SSH private key, signing via `ssh-keygen -Y sign` (program
/// overridable by `gpg.ssh.program`) in git's `git` namespace.
pub(crate) struct CliSigner {
	program: String,
	key_path: PathBuf,
	key_file: Option<Arc<File>>,
	key_use: tokio::sync::Mutex<()>,
	cwd: PathBuf,
	cwd_directory: Dir,
}

impl CliSigner {
	/// Resolve the signing program (`gpg.ssh.program`, default `ssh-keygen`) and key from
	/// `--signing-key <path>`, or git config `user.signingkey` when the flag is absent, and confirm the
	/// file exists. Errors when neither key is set — signing needs a key.
	///
	/// A leading `~`/`~/` is expanded against `$HOME` (as git and ssh do — SSH signing keys commonly
	/// live at `~/.ssh/...`). An otherwise-relative key path (from either source) resolves against
	/// `cwd`, the effective working directory — so `gta -C <dir>` behaves as if started in `<dir>`,
	/// matching git.
	/// Resolve an SSH signer whose relative key and program are anchored to `cwd_directory`.
	pub(crate) async fn resolve_in<F: FileStore, H: HashAlgorithm>(
		repo: &Repository<F, H>,
		signing_key: Option<PathBuf>,
		cwd: &Path,
		cwd_directory: &Dir,
	) -> Result<Self> {
		let cwd = absolute_signer_cwd(cwd)?;
		let config = repo.effective_config().await.ok();
		let program = config
			.as_ref()
			.and_then(|config| config.get_string("gpg", Some("ssh"), "program"))
			.unwrap_or(DEFAULT_SSH_PROGRAM)
			.to_owned();
		validate_retained_signer_program(Path::new(&program))?;
		let configured = match signing_key {
			Some(path) => path,
			None => {
				let configured = config
					.as_ref()
					.and_then(|config| config.get_string("user", None, "signingkey"))
					.ok_or_else(|| {
						anyhow!("no signing key: pass --signing-key <path> or set git config `user.signingkey`")
					})?;
				PathBuf::from(configured)
			}
		};
		// Expand `~` first (yielding an absolute path). Otherwise resolve relative spelling through the
		// retained directory so symlink-hidden traversal cannot change meaning after a namespace move.
		let configured = expand_tilde(configured)?;
		validate_windows_absolute_prefix(&configured)?;
		reject_retained_parent_path(&configured, "signing key")?;
		let (key_path, key_file) = retain_signing_key(&configured, &cwd, cwd_directory).await?;
		Ok(Self {
			program,
			key_path,
			key_file: key_file.map(Arc::new),
			key_use: tokio::sync::Mutex::new(()),
			cwd,
			cwd_directory: cwd_directory
				.try_clone()
				.context("retaining signer working directory")?,
		})
	}

	/// The OpenSSH public-key line for this signing key — what a trust document enrols so the key can
	/// be trusted to verify what it signs.
	///
	/// `user.signingkey` commonly points straight at a public key file (`~/.ssh/id_ed25519.pub`); when
	/// it does, that line *is* the answer, so read it directly. Otherwise the path is a private key and
	/// we derive its public half with `ssh-keygen -y` (which would fail on a public key).
	pub(crate) async fn public_line(&self) -> Result<String> {
		let _key_use = self.key_use.lock().await;
		let contents = read_signing_key(&self.key_path, &self.cwd, self.key_file.clone()).await?;
		if let Some(line) = public_key_line(&contents) {
			return Ok(line);
		}

		let (command, mut file_guards) = signer_program(&self.program, &self.cwd, &self.cwd_directory)
			.await?
			.into_command()
			.await?;
		let (mut command, key_path, key_guard) = self.configure_key_argument(command).await?;
		file_guards.extend(key_guard);
		command.arg("-y").arg("-f").arg(key_path);
		let (mut command, _cwd_guard) =
			configure_signer_current_dir(command, &self.cwd, &self.cwd_directory)
				.await
				.with_context(|| format!("retaining signer working directory {}", self.cwd.display()))?;
		command.kill_on_drop(true);
		let output = command
			.output()
			.await
			.with_context(|| format!("running `{} -y` (is it installed?)", self.program))?;
		drop(file_guards);
		if !output.status.success() {
			bail!(
				"`{} -y` failed: {}",
				self.program,
				String::from_utf8_lossy(&output.stderr).trim()
			);
		}
		Ok(
			String::from_utf8(output.stdout)
				.context("ssh-keygen public-key output was not UTF-8")?
				.trim_end()
				.to_owned(),
		)
	}

	async fn configure_key_argument(
		&self,
		command: StdCommand,
	) -> Result<(StdCommand, PathBuf, Option<ProcessFileGuard>)> {
		let Some(file) = self.key_file.clone() else {
			return Ok((command, self.key_path.clone(), None));
		};
		let display = self.cwd.join(&self.key_path);
		let (command, path, guard) = configure_retained_process_file(command, file, display.clone())
			.await
			.with_context(|| format!("retaining signing key {}", display.display()))?;
		Ok((command, path, Some(guard)))
	}
}

impl Signer for CliSigner {
	async fn sign(&self, payload: &[u8]) -> Result<String> {
		let _key_use = self.key_use.lock().await;
		let (command, mut file_guards) = signer_program(&self.program, &self.cwd, &self.cwd_directory)
			.await?
			.into_command()
			.await?;
		let (mut command, key_path, key_guard) = self.configure_key_argument(command).await?;
		file_guards.extend(key_guard);
		command
			.arg("-Y")
			.arg("sign")
			.arg("-n")
			.arg("git")
			.arg("-f")
			.arg(key_path);
		run_signer(
			command,
			payload,
			&format!("{} -Y sign", self.program),
			&self.cwd,
			&self.cwd_directory,
			file_guards,
		)
		.await
	}
}

async fn retain_signing_key(
	key_path: &Path,
	cwd: &Path,
	cwd_directory: &Dir,
) -> Result<(PathBuf, Option<File>)> {
	let display_path = cwd.join(key_path);
	if key_path.is_relative() {
		let directory = cwd_directory
			.try_clone()
			.context("retaining signer working directory")?;
		let key_path = key_path.to_path_buf();
		let retained = tokio::task::spawn_blocking(move || {
			// Select the actual resource first. Canonicalization then supplies a stable contained
			// spelling, and the second open proves that no namespace replacement changed what it
			// denotes between those steps.
			let selected = open_process_file(&directory, &key_path)?;
			let key_path = directory.canonicalize(key_path)?;
			let parent = key_path.parent().unwrap_or_else(|| Path::new(""));
			let mut key_directory = directory;
			for component in parent.components() {
				match component {
					Component::CurDir => {}
					Component::ParentDir => {
						return Err(std::io::Error::new(
							std::io::ErrorKind::InvalidInput,
							"resolved signing key escaped the retained worktree",
						));
					}
					Component::Normal(name) => {
						key_directory = key_directory.open_dir(name)?;
					}
					Component::Prefix(_) | Component::RootDir => {
						return Err(std::io::Error::new(
							std::io::ErrorKind::InvalidInput,
							"resolved signing key was not relative to the retained worktree",
						));
					}
				}
			}
			let name = key_path.file_name().ok_or_else(|| {
				std::io::Error::new(
					std::io::ErrorKind::InvalidInput,
					"signing key path has no file name",
				)
			})?;
			let name = PathBuf::from(name);
			let visible = open_process_file(&key_directory, &name)?;
			if file_identity(&visible)? != file_identity(&selected)? {
				return Err(std::io::Error::new(
					std::io::ErrorKind::AlreadyExists,
					"signing key changed while it was being retained",
				));
			}
			Ok((key_path, selected))
		})
		.await
		.context("checking retained signing key")?;
		let retained = match retained {
			Ok(retained) => retained,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				bail!("signing key not found: {}", display_path.display());
			}
			Err(error) => {
				return Err(error)
					.with_context(|| format!("checking signing key {}", display_path.display()));
			}
		};
		let (key_path, file) = retained;
		return Ok((key_path, Some(file)));
	}
	let exists = tokio::fs::try_exists(key_path)
		.await
		.with_context(|| format!("checking signing key {}", display_path.display()))?;
	if !exists {
		bail!("signing key not found: {}", display_path.display());
	}
	Ok((key_path.to_path_buf(), None))
}

async fn read_signing_key(
	key_path: &Path,
	cwd: &Path,
	key_file: Option<Arc<File>>,
) -> Result<String> {
	let display_path = cwd.join(key_path);
	if let Some(file) = key_file {
		return tokio::task::spawn_blocking(move || {
			let mut file = file.try_clone()?;
			file.seek(SeekFrom::Start(0))?;
			let mut contents = String::new();
			file.read_to_string(&mut contents)?;
			Ok::<_, std::io::Error>(contents)
		})
		.await
		.context("reading retained signing key")?
		.with_context(|| format!("reading signing key {}", display_path.display()));
	}
	tokio::fs::read_to_string(key_path)
		.await
		.with_context(|| format!("reading signing key {}", display_path.display()))
}

/// A [`Signer`] backed by GnuPG, signing via `gpg --detach-sign --armor` (program overridable by
/// `gpg.program`) — a binary detached OpenPGP signature over the object bytes, exactly what git's
/// `gpgsig` carries and what gitana's trust core verifies. Passphrase handling is gpg-agent's, as with
/// stock `git commit -S`.
pub(crate) struct GpgSigner {
	program: String,
	/// The signing key selector (`--signing-key <keyid>` or `user.signingkey`), passed to gpg's
	/// `--local-user`; `None` lets gpg pick its default signing key.
	key: Option<String>,
	cwd: PathBuf,
	cwd_directory: Dir,
}

impl GpgSigner {
	/// Resolve the signing program (`gpg.program`, default `gpg`) and key selector. Unlike the SSH
	/// path, `user.signingkey`/`--signing-key` is an OpenPGP key id or fingerprint (not a file path),
	/// and is optional — gpg falls back to its own default signing key.
	async fn resolve<F: FileStore, H: HashAlgorithm>(
		repo: &Repository<F, H>,
		signing_key: Option<PathBuf>,
		cwd: &Path,
		cwd_directory: &Dir,
	) -> Result<Self> {
		let cwd = absolute_signer_cwd(cwd)?;
		let config = repo.effective_config().await.ok();
		// git prefers the per-format `gpg.openpgp.program`, falling back to the legacy `gpg.program`.
		let program = config
			.as_ref()
			.and_then(|config| {
				config
					.get_string("gpg", Some("openpgp"), "program")
					.or_else(|| config.get_string("gpg", None, "program"))
			})
			.unwrap_or(DEFAULT_GPG_PROGRAM)
			.to_owned();
		validate_retained_signer_program(Path::new(&program))?;
		let key = match signing_key {
			Some(key) => Some(key.into_os_string().into_string().map_err(|_| {
				anyhow!("--signing-key is not valid UTF-8 (an OpenPGP key id is expected)")
			})?),
			None => config
				.as_ref()
				.and_then(|config| config.get_string("user", None, "signingkey"))
				.map(str::to_owned),
		};
		Ok(Self {
			program,
			key,
			cwd,
			cwd_directory: cwd_directory
				.try_clone()
				.context("retaining signer working directory")?,
		})
	}
}

/// Convert the command's effective cwd to a stable absolute spelling before combining it with
/// relative signer resources. In particular, `gta -C repo` must not pass `repo/key` to a child that
/// is already started in `repo`.
fn absolute_signer_cwd(cwd: &Path) -> Result<PathBuf> {
	std::path::absolute(cwd)
		.with_context(|| format!("resolving signer working directory {}", cwd.display()))
}

/// A resolved signer program plus any file capability that must remain live through spawn.
struct SignerProgram {
	path: PathBuf,
	file: Option<File>,
}

impl SignerProgram {
	async fn into_command(self) -> Result<(StdCommand, Vec<ProcessFileGuard>)> {
		let Self { path, file } = self;
		let command = StdCommand::new(&path);
		let Some(file) = file else {
			return Ok((command, Vec::new()));
		};
		let (command, _, guard) =
			configure_retained_process_file(command, Arc::new(file), path.clone())
				.await
				.with_context(|| format!("retaining signing program {}", path.display()))?;
		Ok((command, vec![guard]))
	}
}

/// Resolve a path-bearing relative signer within the retained worktree before spawning it.
///
/// Unix cannot portably execute an already-open descriptor, so path-bearing relative programs fail
/// closed there. On Windows the resolved executable is opened without write or delete sharing and
/// retained until `CreateProcess` has consumed its absolute path. Bare names intentionally retain
/// PATH lookup and absolute programs retain their spelling.
async fn signer_program(program: &str, cwd: &Path, cwd_directory: &Dir) -> Result<SignerProgram> {
	let program_path = Path::new(program);
	validate_windows_absolute_prefix(program_path)?;
	let path_bearing = is_path_bearing_relative(program_path);
	if !path_bearing {
		return Ok(SignerProgram {
			path: program_path.to_owned(),
			file: None,
		});
	}

	#[cfg(windows)]
	{
		let directory = cwd_directory
			.try_clone()
			.context("retaining signer program directory")?;
		let requested = program_path.to_owned();
		let cwd = cwd.to_owned();
		let (path, file) = tokio::task::spawn_blocking(move || {
			let requested = select_windows_signer_program(&directory, &requested)?;
			let selected = open_process_file(&directory, &requested)?;
			let resolved = directory.canonicalize(&requested)?;
			let visible = open_process_file(&directory, &resolved)?;
			if file_identity(&visible)? != file_identity(&selected)? {
				return Err(std::io::Error::new(
					std::io::ErrorKind::AlreadyExists,
					"signing program changed while it was being retained",
				));
			}
			Ok::<_, std::io::Error>((cwd.join(resolved), selected))
		})
		.await
		.context("joining signer-program retention worker")?
		.with_context(|| format!("retaining signing program `{program}` within the worktree"))?;
		Ok(SignerProgram {
			path,
			file: Some(file),
		})
	}

	#[cfg(all(unix, not(target_os = "fuchsia")))]
	{
		let _ = (cwd, cwd_directory);
		bail!(
			"cannot use path-bearing relative signing program `{program}` on Unix; use a bare PATH name or an absolute path"
		)
	}

	#[cfg(not(any(windows, all(unix, not(target_os = "fuchsia")))))]
	{
		let _ = cwd_directory;
		Ok(SignerProgram {
			path: resolve_signer_program(program, cwd, false)?,
			file: None,
		})
	}
}

fn is_path_bearing_relative(path: &Path) -> bool {
	path.is_relative()
		&& path
			.parent()
			.is_some_and(|parent| !parent.as_os_str().is_empty())
}

/// Select the same path-bearing executable spelling that Rust's Windows `Command` resolver will
/// prefer. Rust appends `.exe` as a suffix, even when another extension is already present, and
/// falls back to the literal spelling only when that suffixed namespace entry is absent.
#[cfg(any(test, windows))]
fn windows_implicit_exe_candidate(program: &Path) -> Option<PathBuf> {
	let bytes = program.as_os_str().as_encoded_bytes();
	if bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".exe") {
		return None;
	}
	let mut candidate = program.as_os_str().to_owned();
	candidate.push(".exe");
	Some(PathBuf::from(candidate))
}

#[cfg(any(test, windows))]
fn select_windows_signer_program(directory: &Dir, requested: &Path) -> std::io::Result<PathBuf> {
	let Some(candidate) = windows_implicit_exe_candidate(requested) else {
		return Ok(requested.to_owned());
	};
	match directory.symlink_metadata(&candidate) {
		Ok(_) => Ok(candidate),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(requested.to_owned()),
		Err(error) => Err(error),
	}
}

#[cfg(any(test, not(all(unix, not(target_os = "fuchsia")))))]
fn resolve_signer_program(
	program: &str,
	cwd: &Path,
	resolve_path_bearing: bool,
) -> Result<PathBuf> {
	let program = Path::new(program);
	validate_windows_absolute_prefix(program)?;
	if resolve_path_bearing
		&& program.is_relative()
		&& program
			.parent()
			.is_some_and(|parent| !parent.as_os_str().is_empty())
	{
		return Ok(cwd.join(program));
	}
	Ok(program.to_owned())
}

fn validate_retained_signer_program(program: &Path) -> Result<()> {
	reject_retained_parent_path(program, "signing program")?;
	validate_windows_absolute_prefix(program)?;
	#[cfg(all(unix, not(target_os = "fuchsia")))]
	if is_path_bearing_relative(program) {
		bail!(
			"cannot use path-bearing relative signing program `{}` on Unix; use a bare PATH name or an absolute path",
			program.display()
		);
	}
	Ok(())
}

/// Windows drive-relative paths (`C:tool.exe`) consult process-global per-drive state and are
/// therefore not anchored by either the retained directory or its absolute display spelling.
#[cfg(windows)]
fn validate_windows_absolute_prefix(path: &Path) -> Result<()> {
	if matches!(path.components().next(), Some(Component::Prefix(_))) && !path.is_absolute() {
		bail!("cannot use drive-relative signing resource on Windows");
	}
	Ok(())
}

#[cfg(not(windows))]
fn validate_windows_absolute_prefix(_path: &Path) -> Result<()> {
	Ok(())
}

/// A retained directory stabilizes itself and paths below it, but Unix `..` follows whichever parent
/// owns the directory at process-spawn time. Reject parent-relative signer resources rather than
/// allowing a cross-parent rename to redirect an irreversible signed merge.
fn reject_retained_parent_path(path: &Path, what: &str) -> Result<()> {
	if path.is_relative()
		&& path
			.components()
			.any(|component| component == Component::ParentDir)
	{
		bail!("cannot use parent-relative {what} from a retained worktree");
	}
	Ok(())
}

impl Signer for GpgSigner {
	async fn sign(&self, payload: &[u8]) -> Result<String> {
		let (mut command, file_guards) = signer_program(&self.program, &self.cwd, &self.cwd_directory)
			.await?
			.into_command()
			.await?;
		command.arg("--detach-sign").arg("--armor");
		if let Some(key) = &self.key {
			command.arg("--local-user").arg(key);
		}
		run_signer(
			command,
			payload,
			&format!("{} --detach-sign", self.program),
			&self.cwd,
			&self.cwd_directory,
			file_guards,
		)
		.await
	}
}

/// The signer a [`LazyCliSigner`] resolves to, per `gpg.format`.
enum ResolvedSigner {
	Ssh(CliSigner),
	Gpg(GpgSigner),
}

impl Signer for ResolvedSigner {
	async fn sign(&self, payload: &[u8]) -> Result<String> {
		match self {
			Self::Ssh(signer) => signer.sign(payload).await,
			Self::Gpg(signer) => signer.sign(payload).await,
		}
	}
}

/// A lazily-resolved signer: it reads `gpg.format`, loads the signing key, and runs the signing
/// program only on the first `sign` call. So an operation that records **no** commit — a no-op
/// `gta commit`, a fast-forward `merge`/`pull`, an up-to-date `rebase` — never touches signing config
/// at all (it must not fail on an unsupported `gpg.format` or a missing key when nothing is signed),
/// while one that records several (a rebase replay) resolves once and reuses. Every history operation
/// is handed this behind an `Option<&LazyCliSigner>`.
pub(crate) struct LazyCliSigner<'a, F: FileStore, H: HashAlgorithm> {
	repo: &'a Repository<F, H>,
	signing_key: Option<PathBuf>,
	cwd: PathBuf,
	cwd_directory: Dir,
	resolved: tokio::sync::OnceCell<ResolvedSigner>,
}

impl<'a, F: FileStore, H: HashAlgorithm> LazyCliSigner<'a, F, H> {
	/// A lazy signer whose relative paths are resolved from a retained working-directory capability.
	pub(crate) fn new_in(
		repo: &'a Repository<F, H>,
		signing_key: Option<PathBuf>,
		cwd: PathBuf,
		cwd_directory: &Dir,
	) -> Result<Self> {
		Ok(Self {
			repo,
			signing_key,
			cwd,
			cwd_directory: cwd_directory
				.try_clone()
				.context("retaining signer working directory")?,
			resolved: tokio::sync::OnceCell::new(),
		})
	}

	/// Resolve the signer per `gpg.format`, matching git: `ssh` → SSHSIG; `openpgp` or **unset** →
	/// OpenPGP (git's default). Any other format is refused. Called once, the first time a commit is
	/// actually signed — deferring the config read and key load off the no-commit paths.
	async fn resolve(&self) -> Result<ResolvedSigner> {
		let config = self.repo.effective_config().await?;
		let format = config.get_string("gpg", None, "format").map(str::to_owned);
		match format.as_deref() {
			Some("ssh") => Ok(ResolvedSigner::Ssh(
				CliSigner::resolve_in(
					self.repo,
					self.signing_key.clone(),
					&self.cwd,
					&self.cwd_directory,
				)
				.await?,
			)),
			Some("openpgp") | None => Ok(ResolvedSigner::Gpg(
				GpgSigner::resolve(
					self.repo,
					self.signing_key.clone(),
					&self.cwd,
					&self.cwd_directory,
				)
				.await?,
			)),
			Some(other) => bail!(
				"cannot sign: git config `gpg.format` is `{other}`; gitana signs with `ssh` or `openpgp`"
			),
		}
	}
}

impl<F: FileStore, H: HashAlgorithm> Signer for LazyCliSigner<'_, F, H> {
	async fn sign(&self, payload: &[u8]) -> Result<String> {
		let signer = self.resolved.get_or_try_init(|| self.resolve()).await?;
		signer.sign(payload).await
	}
}

/// Config-driven signer for an operation that retains its exact working-directory capability.
/// Relative signer programs and SSH keys contained by that directory remain bound through process
/// creation; parent-relative resources fail closed when the signer is resolved.
pub(crate) async fn config_signer_in<'a, F: FileStore, H: HashAlgorithm>(
	repo: &'a Repository<F, H>,
	cwd: &Path,
	cwd_directory: &Dir,
) -> Result<Option<LazyCliSigner<'a, F, H>>> {
	if !config_requests_signing(repo).await? {
		return Ok(None);
	}
	Ok(Some(LazyCliSigner::new_in(
		repo,
		None,
		cwd.to_path_buf(),
		cwd_directory,
	)?))
}

/// Whether git config `commit.gpgSign` requests commit signing (see [`config_requests_gpgsign`]).
pub(crate) async fn config_requests_signing<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<bool> {
	config_requests_gpgsign(repo, "commit").await
}

/// Whether git config `tag.gpgSign` requests signing of annotated tags (`gta tag -a` with no explicit
/// `-s`). The tag analog of [`config_requests_signing`], and equally fails *closed*.
pub(crate) async fn config_requests_tag_signing<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
) -> Result<bool> {
	config_requests_gpgsign(repo, "tag").await
}

/// Read `<section>.gpgSign` (git's boolean signing switch), defaulting to `false`. A config read/parse
/// error propagates rather than silently dropping to unsigned. `gpg.format` is deferred to
/// [`LazyCliSigner`], so an operation that records nothing never fails on it.
async fn config_requests_gpgsign<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	section: &str,
) -> Result<bool> {
	let config = repo.effective_config().await?;
	Ok(config.get_bool(section, None, "gpgsign")?.unwrap_or(false))
}

/// Expand a leading `~` (`~` or `~/…`) against `$HOME`, as git and ssh do for `user.signingkey` —
/// keys commonly live at `~/.ssh/…`. Any other path (including `~user/…`, which we do not resolve) is
/// returned unchanged. Errors only when a `~` needs expanding but `$HOME` is unset.
fn expand_tilde(path: PathBuf) -> Result<PathBuf> {
	let Some(rest) = path.to_str().and_then(|path| path.strip_prefix('~')) else {
		return Ok(path);
	};
	// `~` alone or `~/…`; `~user/…` (a non-empty, non-`/` remainder) is left for the caller to fail on.
	if !rest.is_empty() && !rest.starts_with('/') {
		return Ok(path);
	}
	let home = std::env::var_os("HOME")
		.ok_or_else(|| anyhow!("cannot expand `~` in signing key path: $HOME is not set"))?;
	// Strip the leading `/` off `rest` so it joins as a relative segment onto $HOME.
	Ok(PathBuf::from(home).join(rest.strip_prefix('/').unwrap_or(rest)))
}

/// If `contents` is an OpenSSH public key (its first non-blank line begins with a public key-type
/// token, as in `authorized_keys`), return that line. A private key file (`-----BEGIN … PRIVATE
/// KEY-----`) yields `None`.
pub(crate) fn public_key_line(contents: &str) -> Option<String> {
	const KEY_TYPE_PREFIXES: [&str; 3] = ["ssh-", "ecdsa-", "sk-"];
	let first = contents
		.lines()
		.find(|line| !line.trim().is_empty())?
		.trim();
	KEY_TYPE_PREFIXES
		.iter()
		.any(|prefix| first.starts_with(prefix))
		.then(|| first.to_owned())
}

#[cfg(test)]
mod tests {
	use super::*;

	const PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample admin@example.com";

	#[test]
	fn recognises_a_public_key_line() {
		assert_eq!(public_key_line(&format!("{PUB}\n")).as_deref(), Some(PUB));
		assert_eq!(
			public_key_line("ecdsa-sha2-nistp256 AAAA... e@x").as_deref(),
			Some("ecdsa-sha2-nistp256 AAAA... e@x")
		);
	}

	#[test]
	fn rejects_a_private_key_file() {
		let private =
			"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1r...\n-----END OPENSSH PRIVATE KEY-----\n";
		assert_eq!(public_key_line(private), None);
	}

	#[test]
	fn expands_a_leading_tilde_against_home() {
		let home = PathBuf::from(std::env::var_os("HOME").expect("HOME set in the test environment"));
		// `~/…` expands under $HOME; a bare `~` is $HOME itself.
		assert_eq!(
			expand_tilde(PathBuf::from("~/.ssh/id_ed25519")).unwrap(),
			home.join(".ssh/id_ed25519")
		);
		assert_eq!(expand_tilde(PathBuf::from("~")).unwrap(), home);
	}

	#[test]
	fn leaves_other_paths_unchanged() {
		// Absolute, ordinary-relative, and unsupported `~user/…` paths pass through verbatim.
		for path in ["/abs/key", "rel/key", "~user/key"] {
			assert_eq!(
				expand_tilde(PathBuf::from(path)).unwrap(),
				PathBuf::from(path)
			);
		}
	}

	#[test]
	fn retained_signer_paths_reject_parent_traversal() {
		for path in ["../signer", "keys/../../signing-key"] {
			let error = reject_retained_parent_path(Path::new(path), "signer resource").unwrap_err();
			assert!(error.to_string().contains("parent-relative"), "{error}");
		}
		for path in ["signer", "./signer", "tools/signer", "/outside/signer"] {
			reject_retained_parent_path(Path::new(path), "signer resource").unwrap();
		}
	}

	#[cfg(all(unix, not(target_os = "fuchsia")))]
	#[test]
	fn unix_signer_programs_reject_relative_paths() {
		for program in ["./signer", "tools/signer"] {
			let error = validate_retained_signer_program(Path::new(program)).unwrap_err();
			assert!(
				error.to_string().contains("path-bearing relative"),
				"{error}"
			);
		}
		for program in ["ssh-keygen", "/usr/bin/ssh-keygen"] {
			validate_retained_signer_program(Path::new(program)).unwrap();
		}
	}

	#[test]
	fn windows_resolution_anchors_only_path_bearing_relative_programs() {
		let cwd = Path::new("/module");
		assert_eq!(
			resolve_signer_program("./signer.exe", cwd, true).unwrap(),
			cwd.join("signer.exe")
		);
		assert_eq!(
			resolve_signer_program("tools/signer.exe", cwd, true).unwrap(),
			cwd.join("tools/signer.exe")
		);
		assert_eq!(
			resolve_signer_program("ssh-keygen", cwd, true).unwrap(),
			PathBuf::from("ssh-keygen")
		);
		assert_eq!(
			resolve_signer_program("./signer.exe", cwd, false).unwrap(),
			PathBuf::from("./signer.exe")
		);
	}

	#[test]
	fn windows_resolution_prefers_an_implicit_exe_entry() {
		use cap_std::ambient_authority;

		let temporary = tempfile::tempdir().unwrap();
		std::fs::create_dir(temporary.path().join("tools")).unwrap();
		std::fs::write(temporary.path().join("tools/signer"), b"literal").unwrap();
		std::fs::write(temporary.path().join("tools/signer.exe"), b"executable").unwrap();
		let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();

		assert_eq!(
			select_windows_signer_program(&directory, Path::new("tools/signer")).unwrap(),
			PathBuf::from("tools/signer.exe")
		);
		std::fs::remove_file(temporary.path().join("tools/signer.exe")).unwrap();
		assert_eq!(
			select_windows_signer_program(&directory, Path::new("tools/signer")).unwrap(),
			PathBuf::from("tools/signer")
		);
		assert_eq!(
			select_windows_signer_program(&directory, Path::new("tools/signer.EXE")).unwrap(),
			PathBuf::from("tools/signer.EXE")
		);
	}

	#[cfg(windows)]
	#[test]
	fn windows_resolution_anchors_native_relative_program_spelling() {
		let cwd = Path::new(r"C:\module");
		assert_eq!(
			resolve_signer_program(r".\signer.exe", cwd, true).unwrap(),
			cwd.join("signer.exe")
		);
		for path in [r"C:signer.exe", r"C:tools\signer.exe"] {
			let error = resolve_signer_program(path, cwd, true).unwrap_err();
			assert!(error.to_string().contains("drive-relative"), "{error}");
		}
		for path in [r"C:\tools\signer.exe", r"\\server\share\signer.exe"] {
			assert_eq!(
				resolve_signer_program(path, cwd, true).unwrap(),
				PathBuf::from(path)
			);
		}
	}

	#[tokio::test]
	async fn retained_signer_makes_a_relative_command_cwd_absolute() {
		use cap_std::ambient_authority;
		use gitana_file_store_local::LocalFileStore;
		use gitana_object::Sha1;
		use gitana_object_store::ObjectStore;

		let process_cwd = std::env::current_dir().unwrap();
		let temporary = tempfile::Builder::new()
			.prefix("gitana-relative-signer-")
			.tempdir_in(&process_cwd)
			.unwrap();
		let repository_path = temporary.path().join("repository");
		let worktree_path = temporary.path().join("worktree");
		std::fs::create_dir(&repository_path).unwrap();
		std::fs::create_dir(&worktree_path).unwrap();
		std::fs::write(
			repository_path.join("config"),
			b"[gpg]\n\tformat = ssh\n[user]\n\tsigningkey = signing-key\n",
		)
		.unwrap();
		std::fs::write(worktree_path.join("signing-key"), b"unused\n").unwrap();
		let relative_worktree = worktree_path.strip_prefix(&process_cwd).unwrap();
		let repository = Repository::<_, Sha1>::new(ObjectStore::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&repository_path, ambient_authority()).unwrap(),
		)));
		let worktree_directory = Dir::open_ambient_dir(&worktree_path, ambient_authority()).unwrap();

		let signer = CliSigner::resolve_in(&repository, None, relative_worktree, &worktree_directory)
			.await
			.unwrap();
		assert!(signer.cwd.is_absolute());
		assert_eq!(signer.key_path, PathBuf::from("signing-key"));
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn retained_signer_uses_the_open_key_after_its_name_is_replaced() {
		use cap_std::ambient_authority;
		use gitana_file_store_local::LocalFileStore;
		use gitana_object::Sha1;
		use gitana_object_store::ObjectStore;

		let temporary = tempfile::tempdir().unwrap();
		let repository_path = temporary.path().join("repository");
		let worktree = temporary.path().join("worktree");
		let signer_program = temporary.path().join("signer.sh");
		std::fs::create_dir(&repository_path).unwrap();
		std::fs::create_dir(&worktree).unwrap();
		std::fs::write(
			repository_path.join("config"),
			format!(
				"[gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tprogram = {}\n[user]\n\tsigningkey = signing-key\n",
				signer_program.display()
			),
		)
		.unwrap();
		std::fs::write(worktree.join("signing-key"), b"retained\n").unwrap();
		write_executable_signer(&signer_program);

		let repository = Repository::<_, Sha1>::new(ObjectStore::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&repository_path, ambient_authority()).unwrap(),
		)));
		let directory = Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap();
		let signer = CliSigner::resolve_in(&repository, None, &worktree, &directory)
			.await
			.unwrap();
		std::fs::rename(worktree.join("signing-key"), worktree.join("retained-key")).unwrap();
		std::fs::write(worktree.join("signing-key"), b"replacement\n").unwrap();
		assert_eq!(signer.sign(b"payload").await.unwrap(), "retained");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn retained_public_key_read_uses_the_open_file_after_replacement() {
		use cap_std::ambient_authority;
		use gitana_file_store_local::LocalFileStore;
		use gitana_object::Sha1;
		use gitana_object_store::ObjectStore;

		let temporary = tempfile::tempdir().unwrap();
		let repository_path = temporary.path().join("repository");
		let worktree = temporary.path().join("worktree");
		std::fs::create_dir(&repository_path).unwrap();
		std::fs::create_dir(&worktree).unwrap();
		std::fs::write(
			repository_path.join("config"),
			"[gpg]\n\tformat = ssh\n[user]\n\tsigningkey = signing-key\n",
		)
		.unwrap();
		std::fs::write(worktree.join("signing-key"), format!("{PUB}\n")).unwrap();
		let repository = Repository::<_, Sha1>::new(ObjectStore::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&repository_path, ambient_authority()).unwrap(),
		)));
		let directory = Dir::open_ambient_dir(&worktree, ambient_authority()).unwrap();
		let signer = CliSigner::resolve_in(&repository, None, &worktree, &directory)
			.await
			.unwrap();

		std::fs::rename(worktree.join("signing-key"), worktree.join("retained-key")).unwrap();
		std::fs::write(
			worktree.join("signing-key"),
			"ssh-ed25519 AAAAReplacement replacement@example.com\n",
		)
		.unwrap();
		assert_eq!(signer.public_line().await.unwrap(), PUB);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn retained_signer_preserves_a_contained_key_symlink_after_a_parent_move() {
		use std::os::unix::fs::symlink;

		use cap_std::ambient_authority;
		use gitana_file_store_local::LocalFileStore;
		use gitana_object::Sha1;
		use gitana_object_store::ObjectStore;

		let temporary = tempfile::tempdir().unwrap();
		let repository_path = temporary.path().join("repository");
		let original_parent = temporary.path().join("original");
		let moved_parent = temporary.path().join("moved");
		let visible = original_parent.join("worktree");
		let retained = moved_parent.join("worktree");
		let signer_program = temporary.path().join("signer.sh");
		std::fs::create_dir(&repository_path).unwrap();
		std::fs::create_dir_all(visible.join("material")).unwrap();
		std::fs::create_dir(&moved_parent).unwrap();
		std::fs::write(
			repository_path.join("config"),
			format!(
				"[gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tprogram = {}\n[user]\n\tsigningkey = keys/signing-key\n",
				signer_program.display()
			),
		)
		.unwrap();
		std::fs::write(visible.join("material/signing-key"), b"retained\n").unwrap();
		write_executable_signer(&signer_program);
		symlink("material", visible.join("keys")).unwrap();
		let directory = Dir::open_ambient_dir(&visible, ambient_authority()).unwrap();
		let repository = Repository::<_, Sha1>::new(ObjectStore::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&repository_path, ambient_authority()).unwrap(),
		)));
		let signer = CliSigner::resolve_in(&repository, None, &visible, &directory)
			.await
			.unwrap();
		assert_eq!(signer.key_path, PathBuf::from("material/signing-key"));

		std::fs::rename(&visible, &retained).unwrap();
		std::fs::create_dir_all(visible.join("material")).unwrap();
		std::fs::write(visible.join("material/signing-key"), b"replacement\n").unwrap();
		symlink("material", visible.join("keys")).unwrap();

		assert_eq!(signer.sign(b"payload").await.unwrap(), "retained");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn retained_signer_rejects_a_key_symlink_that_escapes_after_a_parent_move() {
		use std::os::unix::fs::symlink;

		use cap_std::ambient_authority;
		use gitana_file_store_local::LocalFileStore;
		use gitana_object::Sha1;
		use gitana_object_store::ObjectStore;

		let temporary = tempfile::tempdir().unwrap();
		let repository_path = temporary.path().join("repository");
		let original_parent = temporary.path().join("original");
		let moved_parent = temporary.path().join("moved");
		let visible = original_parent.join("worktree");
		let retained = moved_parent.join("worktree");
		let signer_program = temporary.path().join("signer.sh");
		std::fs::create_dir(&repository_path).unwrap();
		std::fs::create_dir_all(&visible).unwrap();
		std::fs::create_dir_all(original_parent.join("keys")).unwrap();
		std::fs::create_dir_all(moved_parent.join("keys")).unwrap();
		std::fs::write(
			repository_path.join("config"),
			format!(
				"[gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tprogram = {}\n[user]\n\tsigningkey = keys/signing-key\n",
				signer_program.display()
			),
		)
		.unwrap();
		std::fs::write(original_parent.join("keys/signing-key"), b"original\n").unwrap();
		std::fs::write(moved_parent.join("keys/signing-key"), b"replacement\n").unwrap();
		write_executable_signer(&signer_program);
		symlink("../keys", visible.join("keys")).unwrap();
		let directory = Dir::open_ambient_dir(&visible, ambient_authority()).unwrap();
		let repository = Repository::<_, Sha1>::new(ObjectStore::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&repository_path, ambient_authority()).unwrap(),
		)));
		let signer = LazyCliSigner::new_in(&repository, None, visible, &directory).unwrap();

		std::fs::rename(original_parent.join("worktree"), &retained).unwrap();
		let error = signer.sign(b"payload").await.unwrap_err();
		assert!(
			error.to_string().contains("checking signing key"),
			"{error:#}"
		);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn retained_openpgp_signer_rejects_a_relative_program() {
		use cap_std::ambient_authority;
		use gitana_file_store_local::LocalFileStore;
		use gitana_object::Sha1;
		use gitana_object_store::ObjectStore;

		let temporary = tempfile::tempdir().unwrap();
		let repository_path = temporary.path().join("repository");
		let visible = temporary.path().join("visible");
		let retained = temporary.path().join("retained");
		std::fs::create_dir(&repository_path).unwrap();
		std::fs::create_dir(&visible).unwrap();
		std::fs::write(
			repository_path.join("config"),
			b"[gpg]\n\tformat = openpgp\n\tprogram = ./gpg.sh\n",
		)
		.unwrap();
		std::fs::write(visible.join("signer-result"), b"retained\n").unwrap();
		write_executable_gpg(&visible.join("gpg.sh"));
		let directory = Dir::open_ambient_dir(&visible, ambient_authority()).unwrap();
		std::fs::rename(&visible, &retained).unwrap();
		std::fs::create_dir(&visible).unwrap();
		std::fs::write(visible.join("signer-result"), b"replacement\n").unwrap();
		write_executable_gpg(&visible.join("gpg.sh"));

		let repository = Repository::<_, Sha1>::new(ObjectStore::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&repository_path, ambient_authority()).unwrap(),
		)));
		let signer = LazyCliSigner::new_in(&repository, None, visible, &directory).unwrap();
		let error = signer.sign(b"payload").await.unwrap_err();
		assert!(
			error.to_string().contains("path-bearing relative"),
			"{error:#}"
		);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn retained_ssh_signer_rejects_a_relative_program_symlink() {
		use std::os::unix::fs::symlink;

		use cap_std::ambient_authority;
		use gitana_file_store_local::LocalFileStore;
		use gitana_object::Sha1;
		use gitana_object_store::ObjectStore;

		let temporary = tempfile::tempdir().unwrap();
		let repository_path = temporary.path().join("repository");
		let original_parent = temporary.path().join("original");
		let replacement_parent = temporary.path().join("replacement-parent");
		let visible = original_parent.join("worktree");
		let retained = replacement_parent.join("retained");
		std::fs::create_dir(&repository_path).unwrap();
		std::fs::create_dir_all(visible.join("tools")).unwrap();
		std::fs::create_dir(&replacement_parent).unwrap();
		std::fs::write(
			repository_path.join("config"),
			b"[gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tprogram = ./bin/signer.sh\n[user]\n\tsigningkey = signing-key\n",
		)
		.unwrap();
		std::fs::write(visible.join("signing-key"), b"retained\n").unwrap();
		write_executable_signer(&visible.join("tools/signer.sh"));
		symlink("tools", visible.join("bin")).unwrap();
		let directory = Dir::open_ambient_dir(&visible, ambient_authority()).unwrap();
		std::fs::rename(&visible, &retained).unwrap();
		std::fs::create_dir_all(visible.join("tools")).unwrap();
		std::fs::write(visible.join("signing-key"), b"replacement\n").unwrap();
		write_executable_signer(&visible.join("tools/signer.sh"));
		symlink("tools", visible.join("bin")).unwrap();

		let repository = Repository::<_, Sha1>::new(ObjectStore::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&repository_path, ambient_authority()).unwrap(),
		)));
		let signer = LazyCliSigner::new_in(&repository, None, visible, &directory).unwrap();
		let error = signer.sign(b"payload").await.unwrap_err();
		assert!(
			error.to_string().contains("path-bearing relative"),
			"{error:#}"
		);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn retained_signer_rejects_a_program_symlink_that_escapes_after_a_parent_move() {
		use std::os::unix::fs::symlink;

		use cap_std::ambient_authority;
		use gitana_file_store_local::LocalFileStore;
		use gitana_object::Sha1;
		use gitana_object_store::ObjectStore;

		let temporary = tempfile::tempdir().unwrap();
		let repository_path = temporary.path().join("repository");
		let original_parent = temporary.path().join("original");
		let moved_parent = temporary.path().join("moved");
		let visible = original_parent.join("worktree");
		let retained = moved_parent.join("worktree");
		std::fs::create_dir(&repository_path).unwrap();
		std::fs::create_dir_all(&visible).unwrap();
		std::fs::create_dir_all(original_parent.join("signers")).unwrap();
		std::fs::create_dir_all(moved_parent.join("signers")).unwrap();
		std::fs::write(
			repository_path.join("config"),
			b"[gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tprogram = ./bin/signer.sh\n[user]\n\tsigningkey = signing-key\n",
		)
		.unwrap();
		std::fs::write(visible.join("signing-key"), b"retained\n").unwrap();
		symlink("../signers", visible.join("bin")).unwrap();
		write_executable_signer(&original_parent.join("signers/signer.sh"));
		write_executable_signer(&moved_parent.join("signers/signer.sh"));
		let directory = Dir::open_ambient_dir(&visible, ambient_authority()).unwrap();
		std::fs::rename(&visible, &retained).unwrap();

		let repository = Repository::<_, Sha1>::new(ObjectStore::new(LocalFileStore::from_dir(
			Dir::open_ambient_dir(&repository_path, ambient_authority()).unwrap(),
		)));
		let signer = LazyCliSigner::new_in(&repository, None, retained, &directory).unwrap();
		let error = signer.sign(b"payload").await.unwrap_err();
		assert!(
			error.to_string().contains("path-bearing relative"),
			"{error:#}"
		);
	}

	#[cfg(unix)]
	fn write_executable_signer(path: &Path) {
		use std::os::unix::fs::PermissionsExt as _;

		std::fs::write(
			path,
			b"#!/bin/sh\ncat >/dev/null\nwhile [ \"$#\" -gt 0 ]; do\n\tif [ \"$1\" = -f ]; then\n\t\tshift\n\t\tkey=$1\n\tfi\n\tshift\ndone\ncat \"$key\"\n",
		)
		.unwrap();
		let mut permissions = std::fs::metadata(path).unwrap().permissions();
		permissions.set_mode(0o755);
		std::fs::set_permissions(path, permissions).unwrap();
	}

	#[cfg(unix)]
	fn write_executable_gpg(path: &Path) {
		use std::os::unix::fs::PermissionsExt as _;

		std::fs::write(path, b"#!/bin/sh\ncat >/dev/null\ncat signer-result\n").unwrap();
		let mut permissions = std::fs::metadata(path).unwrap().permissions();
		permissions.set_mode(0o755);
		std::fs::set_permissions(path, permissions).unwrap();
	}
}
