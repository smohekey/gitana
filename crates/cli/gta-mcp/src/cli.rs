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
	},
	/// Show the commit history of HEAD (one line per commit).
	Log,
	/// List branches, or create one.
	Branch {
		/// Branch to create. With none, list branches.
		#[arg(long)]
		name: Option<String>,
		/// Revision the new branch points at (default: `HEAD`).
		#[arg(long)]
		start: Option<String>,
	},
	/// List tags, or create a lightweight one.
	Tag {
		/// Tag to create. With none, list tags.
		#[arg(long)]
		name: Option<String>,
		/// Revision the new tag points at (default: `HEAD`).
		#[arg(long)]
		target: Option<String>,
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
		/// Attach a push certificate.
		#[arg(long)]
		signed: bool,
		/// SSH key to sign the push with (default: git config `user.signingkey`).
		#[arg(long = "signing-key", value_name = "path")]
		signing_key: Option<PathBuf>,
		/// Allow a non-fast-forward update.
		#[arg(short = 'f', long)]
		force: bool,
		/// Delete a remote branch instead of pushing.
		#[arg(long, value_name = "branch")]
		delete: Option<String>,
	},
}

impl Cli {
	async fn dispatch(self) -> Result<()> {
		let cwd = match self.directory {
			Some(dir) => dir,
			None => std::env::current_dir()?,
		};
		match self.command {
			Command::Init { path } => commands::init::run(path.unwrap_or(cwd)).await,
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
			Command::LsFiles => commands::ls_files::run(&cwd).await,
			Command::UpdateRef { name, value } => commands::update_ref::run(&cwd, &name, &value).await,
			Command::SymbolicRef { name, target } => {
				commands::symbolic_ref::run(&cwd, &name, target).await
			}
			Command::Add { pathspecs } => commands::add::run(&cwd, &pathspecs).await,
			Command::Status => commands::status::run(&cwd).await,
			Command::Commit { message } => commands::commit::run(&cwd, &message).await,
			Command::Log => commands::log::run(&cwd).await,
			Command::Branch { name, start } => commands::branch::run(&cwd, name, start).await,
			Command::Tag { name, target } => commands::tag::run(&cwd, name, target).await,
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
			Command::Diff { cached } => commands::diff::run(&cwd, cached).await,
			Command::Clone { url, path } => commands::clone::run(url, path).await,
			Command::Fetch => commands::fetch::run(&cwd).await,
			Command::Pull => commands::pull::run(&cwd).await,
			Command::Push {
				signed,
				signing_key,
				force,
				delete,
			} => commands::push::run(&cwd, signed, signing_key, force, delete).await,
		}
	}
}
