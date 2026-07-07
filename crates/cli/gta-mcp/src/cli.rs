//! The `gta-mcp` argument surface: the same commands as the `gta` CLI, but every command
//! is a clean MCP tool. Commands that take two positional arguments on the `gta` CLI
//! (`update-ref`, `symbolic-ref`, `branch`, `tag`, `switch`, `clone`) use **named**
//! arguments here, since MCP tool calls can't order multiple positionals. All commands
//! delegate to the shared `gta-core` implementations.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use clap_mcp::{ClapMcpToolError, IntoClapMcpToolError};
use gta_core::commands;

/// The clap-mcp output function (named by `#[clap_mcp_output_from]`): drive the parsed
/// command to completion on a fresh current-thread runtime. Handlers print their output to
/// stdout — which clap-mcp captures from the re-executed subprocess in MCP mode — so this
/// returns no extra text on success and maps any error to an MCP tool error.
pub(crate) fn execute(cli: Cli) -> std::result::Result<String, McpError> {
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|error| McpError(error.into()))?;
	runtime.block_on(cli.dispatch()).map_err(McpError)?;
	Ok(String::new())
}

/// Wraps a gta error so clap-mcp can render it as an MCP tool error (`is_error: true`).
pub(crate) struct McpError(pub(crate) anyhow::Error);

impl IntoClapMcpToolError for McpError {
	fn into_tool_error(self) -> ClapMcpToolError {
		ClapMcpToolError::text(format!("{:#}", self.0))
	}
}

/// The `gta-mcp` command line. Deriving `ClapMcp` exposes each subcommand as an MCP tool
/// (`--mcp` over stdio, `--mcp-http` over HTTP); each tool call re-executes `gta-mcp` as a
/// subprocess (`reinvocation_safe = false`). Operations touch a shared working tree, so
/// tool calls are not parallel-safe.
#[derive(Parser, clap_mcp::ClapMcp)]
#[command(
	name = "gta-mcp",
	version,
	about = "MCP server for the gta toolchain",
	long_about = "MCP server for the gta toolchain.\n\nRun `--mcp` to serve the gta \
	              commands as MCP tools over stdio, or `--mcp-http <addr>` over HTTP. With \
	              no flag, runs a single command and exits, like `gta`."
)]
#[clap_mcp(reinvocation_safe = false, parallel_safe = false)]
#[clap_mcp_output_from = "execute"]
pub(crate) struct Cli {
	/// Run as if started in `<dir>`.
	#[arg(short = 'C', value_name = "dir", global = true)]
	directory: Option<PathBuf>,
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand, clap_mcp::ClapMcp)]
#[clap_mcp(schema_only)]
enum Command {
	/// Create an empty repository.
	Init {
		/// Directory to create the repository in (default: current directory).
		path: Option<PathBuf>,
		/// Object hash format for the new repository.
		#[arg(long, value_name = "format", default_value = "sha256")]
		object_format: String,
	},
	/// Compute (and optionally write) an object id.
	HashObject {
		/// Object type.
		#[arg(short = 't', value_name = "type", default_value = "blob")]
		kind: String,
		/// Write the object into the object store.
		#[arg(short = 'w')]
		write: bool,
		/// Read the object from standard input.
		#[arg(long)]
		stdin: bool,
		/// File to hash (unless `--stdin`).
		file: Option<PathBuf>,
	},
	/// Show an object's type (`-t`), size (`-s`), or content (`-p`).
	CatFile {
		#[arg(short = 't', group = "mode")]
		show_type: bool,
		#[arg(short = 's', group = "mode")]
		show_size: bool,
		#[arg(short = 'p', group = "mode")]
		pretty: bool,
		/// The object (oid, abbreviation, or revision).
		object: String,
	},
	/// List the contents of a tree.
	LsTree {
		/// Recurse into subtrees, listing blobs.
		#[arg(short = 'r')]
		recursive: bool,
		/// A tree, commit, or revision.
		treeish: String,
	},
	/// Resolve a revision to an object id.
	RevParse {
		/// The revision (oid, abbreviation, ref, `HEAD`, `~`/`^`/`^{}`).
		spec: String,
	},
	/// List commits reachable from a revision, newest first.
	RevList {
		/// The starting revision.
		spec: String,
	},
	/// Find the best common ancestor(s) of two or more commits.
	MergeBase {
		/// Print all merge bases, not just one.
		#[arg(long)]
		all: bool,
		/// Test if the first commit is an ancestor of the second (exit 0/1, no output).
		#[arg(long = "is-ancestor")]
		is_ancestor: bool,
		/// The commits (two or more).
		#[arg(long, required = true)]
		commits: Vec<String>,
	},
	/// List the paths tracked in the index.
	LsFiles,
	/// Point a ref at an object.
	UpdateRef {
		/// The ref name (e.g. `refs/heads/main`).
		#[arg(long)]
		name: String,
		/// The new value (a revision).
		#[arg(long)]
		value: String,
	},
	/// Read or set a symbolic ref.
	SymbolicRef {
		/// The symbolic ref name (e.g. `HEAD`).
		#[arg(long)]
		name: String,
		/// If given, set the ref to this target instead of reading it.
		#[arg(long)]
		target: Option<String>,
	},
	/// Stage file contents into the index.
	Add {
		/// Pathspecs to stage (files, directories, or `.`).
		#[arg(required = true)]
		pathspecs: Vec<String>,
	},
	/// Show the working-tree status (porcelain v1).
	Status,
	/// Record a commit from the staged index.
	Commit {
		/// Commit message.
		#[arg(short = 'm', long = "message")]
		message: String,
		/// Sign the commit with SSH (default: git config `commit.gpgsign`).
		#[arg(short = 'S', long = "gpg-sign")]
		sign: bool,
		/// Do not sign, overriding `commit.gpgsign`.
		#[arg(long = "no-gpg-sign", conflicts_with = "sign")]
		no_sign: bool,
		/// SSH key to sign with (default: git config `user.signingkey`).
		#[arg(long = "signing-key", value_name = "path")]
		signing_key: Option<PathBuf>,
	},
	/// Merge a commit into the current branch (fast-forward or a true merge commit).
	Merge {
		/// The commit (or branch) to merge into the current branch (omit with --abort/--continue).
		#[arg(long)]
		commit: Option<String>,
		/// Merge commit message.
		#[arg(short = 'm', long = "message")]
		message: Option<String>,
		/// Always create a merge commit, even when a fast-forward is possible.
		#[arg(long = "no-ff")]
		no_ff: bool,
		/// Refuse to merge unless a fast-forward is possible.
		#[arg(long = "ff-only")]
		ff_only: bool,
		/// Abort an in-progress merge, restoring the pre-merge state.
		#[arg(long = "abort")]
		abort: bool,
		/// Conclude an in-progress merge after resolving its conflicts.
		#[arg(long = "continue")]
		continue_: bool,
	},
	/// Re-apply a commit's change onto the current branch (preserving its author).
	CherryPick {
		/// The commit to cherry-pick (omit with --abort/--continue).
		#[arg(long)]
		commit: Option<String>,
		/// Abort an in-progress cherry-pick, restoring the pre-pick state.
		#[arg(long = "abort")]
		abort: bool,
		/// Conclude an in-progress cherry-pick after resolving its conflicts.
		#[arg(long = "continue")]
		continue_: bool,
	},
	/// Record a new commit that undoes a previous commit's change.
	Revert {
		/// The commit to revert (omit with --abort/--continue).
		#[arg(long)]
		commit: Option<String>,
		/// Abort an in-progress revert, restoring the pre-revert state.
		#[arg(long = "abort")]
		abort: bool,
		/// Conclude an in-progress revert after resolving its conflicts.
		#[arg(long = "continue")]
		continue_: bool,
	},
	/// Replay the current branch's commits onto another base.
	Rebase {
		/// The upstream branch/commit to rebase onto (omit with --abort/--continue/--skip).
		#[arg(long)]
		upstream: Option<String>,
		/// Rebase onto this commit instead of <upstream> (the commits replayed are still
		/// <upstream>..HEAD).
		#[arg(long)]
		onto: Option<String>,
		/// Abort an in-progress rebase, restoring the original branch.
		#[arg(long = "abort")]
		abort: bool,
		/// Continue an in-progress rebase after resolving conflicts.
		#[arg(long = "continue")]
		continue_: bool,
		/// Skip the current commit and continue an in-progress rebase.
		#[arg(long = "skip")]
		skip: bool,
	},
	/// Consolidate loose objects and existing packs into size-bounded packs.
	Repack {
		/// Incremental (geometric) repack: keep the large packs, roll only the small packs and
		/// loose objects into new ones.
		#[arg(long)]
		geometric: bool,
	},
	/// Delete loose objects unreachable from any ref, HEAD, the index, or the reflogs.
	Prune,
	/// Delete unreachable loose objects (prune) then consolidate storage (repack).
	Gc,
	/// Show the commit history of HEAD (one line per commit).
	Log,
	/// Show an object: a commit and its diff, a tag, a tree, or a blob (default: HEAD).
	Show {
		/// The object to show (default: HEAD).
		#[arg(long)]
		object: Option<String>,
	},
	/// Read or write local repository configuration (`.git/config`).
	Config {
		/// Read the value of a key (the default with a key and no value).
		#[arg(long)]
		get: bool,
		/// Print every value of a multi-valued key.
		#[arg(long = "get-all")]
		get_all: bool,
		/// Append a value to a key.
		#[arg(long)]
		add: bool,
		/// Replace all values of a key with a single value.
		#[arg(long = "replace-all")]
		replace_all: bool,
		/// Remove a key.
		#[arg(long)]
		unset: bool,
		/// List all variables as `key=value`.
		#[arg(long)]
		list: bool,
		/// Interpret the read value as a boolean.
		#[arg(long = "bool")]
		as_bool: bool,
		/// Interpret the read value as an integer.
		#[arg(long = "int")]
		as_int: bool,
		/// The dotted key (`section[.subsection].name`).
		#[arg(long)]
		name: Option<String>,
		/// The value to set or add.
		#[arg(long)]
		value: Option<String>,
	},
	/// List branches, or create one.
	Branch {
		/// Branch to create. With none, list branches.
		#[arg(long)]
		name: Option<String>,
		/// Revision the new branch points at (default: `HEAD`).
		#[arg(long)]
		start: Option<String>,
	},
	/// List tags, or create a lightweight, annotated, or signed one.
	Tag {
		/// Tag to create. With none, list tags.
		#[arg(long)]
		name: Option<String>,
		/// Revision the new tag points at (default: `HEAD`).
		#[arg(long)]
		target: Option<String>,
		/// Create an annotated tag object (implied by message/sign).
		#[arg(short = 'a', long = "annotate")]
		annotate: bool,
		/// Sign the annotated tag with SSH (implies annotate; default: git config `tag.gpgSign`).
		#[arg(short = 's', long = "sign")]
		sign: bool,
		/// Do not sign, overriding `tag.gpgSign`.
		#[arg(long = "no-sign", conflicts_with = "sign")]
		no_sign: bool,
		/// Tag message (implies annotate).
		#[arg(short = 'm', long = "message")]
		message: Option<String>,
		/// SSH key to sign with (default: git config `user.signingkey`).
		#[arg(long = "signing-key", value_name = "path")]
		signing_key: Option<PathBuf>,
	},
	/// Switch branches, updating the working tree and HEAD.
	Switch {
		/// Create the branch before switching to it.
		#[arg(short = 'c')]
		create: bool,
		/// Discard local changes that would be overwritten.
		#[arg(short = 'f', long = "force")]
		force: bool,
		/// Branch to switch to.
		#[arg(long)]
		name: String,
		/// Start point for `-c` (default: `HEAD`).
		#[arg(long)]
		start: Option<String>,
	},
	/// Switch branches, or restore working-tree files from a tree-ish or the index.
	Checkout {
		/// Discard local changes that would be overwritten.
		#[arg(short = 'f', long = "force")]
		force: bool,
		/// Branch to switch to, or tree-ish to restore paths from.
		#[arg(long)]
		target: Option<String>,
		/// Paths to restore. When given, restore mode; otherwise branch switch.
		#[arg(long)]
		paths: Vec<String>,
	},
	/// Restore working-tree and/or staged paths from a tree-ish, the index, or `HEAD`.
	Restore {
		/// Restore the working tree (the default when neither target is given).
		#[arg(long)]
		worktree: bool,
		/// Restore the index (staging area).
		#[arg(long)]
		staged: bool,
		/// Tree-ish to restore from (default: the index, or `HEAD` with `staged`).
		#[arg(long)]
		source: Option<String>,
		/// Paths to restore.
		#[arg(long)]
		paths: Vec<String>,
	},
	/// Reset the current branch (and optionally index/working tree) to a commit, or reset paths.
	Reset {
		/// Move `HEAD` only, keeping the index and working tree.
		#[arg(long)]
		soft: bool,
		/// Move `HEAD` and reset the index, keeping the working tree (the default).
		#[arg(long)]
		mixed: bool,
		/// Move `HEAD` and reset both the index and the working tree, discarding changes.
		#[arg(long)]
		hard: bool,
		/// Commit to reset to, default `HEAD`.
		#[arg(long)]
		target: Option<String>,
		/// Paths to reset in the index; does not move `HEAD`.
		#[arg(long)]
		paths: Vec<String>,
	},
	/// Remove tracked files from the index and the working tree.
	Rm {
		/// Remove from the index only, keeping the working-tree file.
		#[arg(long)]
		cached: bool,
		/// Override the up-to-date safety check.
		#[arg(long)]
		force: bool,
		/// Allow removing a tracked directory's contents recursively.
		#[arg(long)]
		recursive: bool,
		/// Show what would be removed without removing it.
		#[arg(long)]
		dry_run: bool,
		/// Paths to remove.
		#[arg(long)]
		pathspecs: Vec<String>,
	},
	/// Move or rename a tracked file or directory (filesystem move plus index update).
	Mv {
		/// Overwrite an existing destination.
		#[arg(long)]
		force: bool,
		/// Show what would be moved without moving it.
		#[arg(long)]
		dry_run: bool,
		/// Report each rename performed.
		#[arg(long)]
		verbose: bool,
		/// One or more sources followed by the destination.
		#[arg(long, required = true)]
		paths: Vec<String>,
	},
	/// Show changes between commits, the index, and the working tree.
	Diff {
		/// Show staged changes (HEAD vs index) instead of unstaged.
		#[arg(long, visible_alias = "staged")]
		cached: bool,
	},
	/// Clone a repository from a Git Smart HTTP remote.
	Clone {
		/// The repository URL (e.g. `http://localhost:8080/acme/app`).
		#[arg(long)]
		url: String,
		/// Directory to clone into (default: the repository slug).
		#[arg(long)]
		path: Option<PathBuf>,
	},
	/// Download new objects from the origin and update remote-tracking refs.
	Fetch,
	/// Fetch the current branch from the origin and fast-forward the working tree.
	Pull,
	/// Push the current branch to the origin.
	Push {
		/// The remote to push to (must be `origin`); defaults to `origin`.
		#[arg(long)]
		repository: Option<String>,
		/// Refspecs to push: `[+]<src>:<dst>`, `<name>` (same-name), or `:<dst>` (delete). None pushes
		/// `HEAD`'s branch (or `remote.origin.push`).
		#[arg(long = "refspec")]
		refspecs: Vec<String>,
		/// Attach a push certificate.
		#[arg(long)]
		signed: bool,
		/// SSH key to sign the push with (default: git config `user.signingkey`).
		#[arg(long = "signing-key", value_name = "path")]
		signing_key: Option<PathBuf>,
		/// Allow a non-fast-forward update.
		#[arg(short = 'f', long)]
		force: bool,
		/// Delete a remote ref instead of pushing (sugar for a `:<ref>` refspec).
		#[arg(long, value_name = "ref")]
		delete: Option<String>,
	},
	/// List, add, remove, or retarget the configured remotes.
	Remote {
		/// With no sub-command, also print each remote's fetch/push URL.
		#[arg(short, long)]
		verbose: bool,
		#[command(subcommand)]
		action: Option<RemoteAction>,
	},
	/// Manage the repository's trust root (the signed `refs/gitana/trust` chain).
	Trust {
		#[command(subcommand)]
		action: TrustAction,
	},
}

/// A `trust` sub-command.
#[derive(Subcommand, clap_mcp::ClapMcp)]
#[clap_mcp(schema_only)]
enum TrustAction {
	/// Bootstrap the trust root: create a self-signed root enrolling the signing key.
	Init {
		/// Enforcement policy for the new root.
		#[arg(long, default_value = "warn")]
		policy: String,
		/// SSH private key to sign with (default: git config `user.signingkey`).
		#[arg(long = "signing-key")]
		signing_key: Option<PathBuf>,
		/// Allow `--policy require` with a single enrolled key (unsafe: losing it locks the repository).
		#[arg(long = "break-glass")]
		break_glass: bool,
		/// Report what bootstrapping would do, without writing anything.
		#[arg(long = "dry-run")]
		dry_run: bool,
	},
	/// Show the current trust policy and enrolled key fingerprints.
	List,
	/// Enrol a public key in the trust root.
	AddKey {
		/// Public key to enrol: a file path or a literal OpenSSH key line / armored OpenPGP certificate.
		#[arg(long)]
		key: String,
		/// SSH private key to sign the update with (default: git config `user.signingkey`).
		#[arg(long = "signing-key")]
		signing_key: Option<PathBuf>,
	},
	/// Remove a key from the trust root.
	RemoveKey {
		/// Key to remove: a `SHA256:…` or OpenPGP hex fingerprint, or a public-key file / OpenSSH line.
		#[arg(long)]
		key: String,
		/// SSH private key to sign the update with (default: git config `user.signingkey`).
		#[arg(long = "signing-key")]
		signing_key: Option<PathBuf>,
		/// Allow leaving a `require` root with a single key (unsafe: losing it locks the repository).
		#[arg(long = "break-glass")]
		break_glass: bool,
	},
	/// Change the enforcement policy.
	SetPolicy {
		/// New policy (off, warn, or require).
		#[arg(long)]
		policy: String,
		/// SSH private key to sign the update with (default: git config `user.signingkey`).
		#[arg(long = "signing-key")]
		signing_key: Option<PathBuf>,
		/// Allow `require` with fewer than two enrolled keys (unsafe: losing a key locks the repository).
		#[arg(long = "break-glass")]
		break_glass: bool,
		/// Report the cutover impact, without writing anything.
		#[arg(long = "dry-run")]
		dry_run: bool,
	},
	/// Adopt the origin's trust root into the local `refs/gitana/trust` (forward-only, verified).
	Sync {
		/// On a first-use bootstrap (no local trust yet), only adopt if the incoming root's bootstrap
		/// was signed by this `SHA256:…` fingerprint (the chain's anchor). Required here: without a
		/// terminal to prompt, an unpinned adoption is refused.
		#[arg(long = "expect")]
		expect: Option<String>,
	},
}

/// A `remote` sub-command. Absent means "list the remotes".
#[derive(Subcommand, clap_mcp::ClapMcp)]
#[clap_mcp(schema_only)]
enum RemoteAction {
	/// Add a remote named <name> for <url>.
	Add {
		#[arg(long)]
		name: String,
		#[arg(long)]
		url: String,
	},
	/// Remove the remote named <name> (and its remote-tracking refs).
	#[command(alias = "rm")]
	Remove {
		#[arg(long)]
		name: String,
	},
	/// Rename the remote <old> to <new>.
	Rename {
		#[arg(long)]
		old: String,
		#[arg(long)]
		new: String,
	},
	/// Change the URL of the remote named <name>.
	SetUrl {
		#[arg(long)]
		name: String,
		#[arg(long)]
		url: String,
	},
}

impl Cli {
	async fn dispatch(self) -> Result<()> {
		let cwd = match self.directory {
			Some(dir) => dir,
			None => std::env::current_dir()?,
		};
		match self.command {
			Command::Init {
				path,
				object_format,
			} => commands::init::run(path.unwrap_or(cwd), &object_format).await,
			Command::HashObject {
				kind,
				write,
				stdin,
				file,
			} => commands::hash_object::run(&cwd, &kind, write, stdin, file).await,
			Command::CatFile {
				show_type,
				show_size,
				pretty,
				object,
			} => commands::cat_file::run(&cwd, show_type, show_size, pretty, &object).await,
			Command::LsTree { recursive, treeish } => {
				commands::ls_tree::run(&cwd, recursive, &treeish).await
			}
			Command::RevParse { spec } => commands::rev_parse::run(&cwd, &spec).await,
			Command::RevList { spec } => commands::rev_list::run(&cwd, &spec).await,
			Command::MergeBase {
				all,
				is_ancestor,
				commits,
			} => commands::merge_base::run(&cwd, all, is_ancestor, commits).await,
			Command::LsFiles => commands::ls_files::run(&cwd).await,
			Command::UpdateRef { name, value } => commands::update_ref::run(&cwd, &name, &value).await,
			Command::SymbolicRef { name, target } => {
				commands::symbolic_ref::run(&cwd, &name, target).await
			}
			Command::Add { pathspecs } => commands::add::run(&cwd, &pathspecs).await,
			Command::Status => commands::status::run(&cwd).await,
			Command::Commit {
				message,
				sign,
				no_sign,
				signing_key,
			} => commands::commit::run(&cwd, &message, sign, no_sign, signing_key).await,
			Command::Merge {
				commit,
				message,
				no_ff,
				ff_only,
				abort,
				continue_,
			} => commands::merge::run(&cwd, commit, message, no_ff, ff_only, abort, continue_).await,
			Command::CherryPick {
				commit,
				abort,
				continue_,
			} => commands::cherry_pick::run(&cwd, commit, abort, continue_).await,
			Command::Revert {
				commit,
				abort,
				continue_,
			} => commands::revert::run(&cwd, commit, abort, continue_).await,
			Command::Rebase {
				upstream,
				onto,
				abort,
				continue_,
				skip,
			} => commands::rebase::run(&cwd, upstream, onto, abort, continue_, skip).await,
			Command::Repack { geometric } => commands::repack::run(&cwd, geometric).await,
			Command::Prune => commands::prune::run(&cwd).await,
			Command::Gc => commands::gc::run(&cwd).await,
			Command::Log => commands::log::run(&cwd).await,
			Command::Show { object } => commands::show::run(&cwd, object).await,
			Command::Config {
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
			} => {
				commands::config::run(
					&cwd,
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
				)
				.await
			}
			Command::Branch { name, start } => commands::branch::run(&cwd, name, start).await,
			Command::Tag {
				name,
				target,
				annotate,
				sign,
				no_sign,
				message,
				signing_key,
			} => {
				commands::tag::run(
					&cwd,
					name,
					target,
					annotate,
					sign,
					no_sign,
					message,
					signing_key,
				)
				.await
			}
			Command::Switch {
				create,
				force,
				name,
				start,
			} => commands::switch::run(&cwd, &name, create, start, force).await,
			Command::Checkout {
				force,
				target,
				paths,
			} => commands::checkout::run(&cwd, force, target, paths).await,
			Command::Restore {
				worktree,
				staged,
				source,
				paths,
			} => commands::restore::run(&cwd, worktree, staged, source, paths).await,
			Command::Reset {
				soft,
				mixed,
				hard,
				target,
				paths,
			} => commands::reset::run(&cwd, soft, mixed, hard, target, paths).await,
			Command::Rm {
				cached,
				force,
				recursive,
				dry_run,
				pathspecs,
			} => commands::rm::run(&cwd, cached, force, recursive, dry_run, pathspecs).await,
			Command::Mv {
				force,
				dry_run,
				verbose,
				paths,
			} => commands::mv::run(&cwd, force, dry_run, verbose, paths).await,
			Command::Diff { cached } => commands::diff::run(&cwd, cached).await,
			Command::Clone { url, path } => commands::clone::run(url, path).await,
			Command::Fetch => commands::fetch::run(&cwd).await.map(|_| ()),
			Command::Pull => commands::pull::run(&cwd).await,
			Command::Push {
				repository,
				refspecs,
				signed,
				signing_key,
				force,
				delete,
			} => {
				// `--repository` is an explicit remote (not git's ambiguous positional), so a non-`origin`
				// value is a mistake, not a refspec shorthand — reject it before it reaches the pusher.
				if let Some(remote) = &repository
					&& remote != "origin"
				{
					anyhow::bail!("gitana has a single remote; --repository must be `origin`");
				}
				commands::push::run(
					&cwd,
					repository,
					refspecs,
					signed,
					signing_key,
					force,
					delete,
				)
				.await
			}
			Command::Remote { verbose, action } => {
				commands::remote::run(&cwd, remote_action(verbose, action)).await
			}
			Command::Trust { action } => commands::trust::run(&cwd, trust_action(action)).await,
		}
	}
}

/// Map the clap `trust` sub-command to the `gta-core` action.
fn trust_action(action: TrustAction) -> commands::trust::Action {
	use commands::trust::Action;
	match action {
		TrustAction::Init {
			policy,
			signing_key,
			break_glass,
			dry_run,
		} => Action::Init {
			policy,
			signing_key,
			break_glass,
			dry_run,
		},
		TrustAction::List => Action::List,
		TrustAction::AddKey { key, signing_key } => Action::AddKey { key, signing_key },
		TrustAction::RemoveKey {
			key,
			signing_key,
			break_glass,
		} => Action::RemoveKey {
			key,
			signing_key,
			break_glass,
		},
		TrustAction::SetPolicy {
			policy,
			signing_key,
			break_glass,
			dry_run,
		} => Action::SetPolicy {
			policy,
			signing_key,
			break_glass,
			dry_run,
		},
		TrustAction::Sync { expect } => Action::Sync { expect },
	}
}

/// Map the clap `remote` sub-command to the `gta-core` action (absent means "list").
fn remote_action(verbose: bool, action: Option<RemoteAction>) -> commands::remote::Action {
	use commands::remote::Action;
	match action {
		None => Action::List { verbose },
		Some(RemoteAction::Add { name, url }) => Action::Add { name, url },
		Some(RemoteAction::Remove { name }) => Action::Remove { name },
		Some(RemoteAction::Rename { old, new }) => Action::Rename { old, new },
		Some(RemoteAction::SetUrl { name, url }) => Action::SetUrl { name, url },
	}
}
