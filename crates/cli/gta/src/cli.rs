use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use gta_core::commands;

/// Parse the command line and run the requested command.
pub async fn run() -> Result<()> {
	Cli::parse().dispatch().await
}

#[derive(Parser)]
#[command(name = "gta", version, about = "A git-compatible, SHA-256 CLI")]
pub(crate) struct Cli {
	/// Run as if started in `<dir>`.
	#[arg(short = 'C', value_name = "dir", global = true)]
	directory: Option<PathBuf>,
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
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
	/// Find the best common ancestor(s) of two or more commits.
	MergeBase {
		/// Print all merge bases, not just one.
		#[arg(long)]
		all: bool,
		/// Test if the first commit is an ancestor of the second (exit 0/1, no output).
		#[arg(long = "is-ancestor")]
		is_ancestor: bool,
		/// The commits (two or more).
		#[arg(required = true)]
		commits: Vec<String>,
	},
	/// List the paths tracked in the index.
	LsFiles,
	/// Point a ref at an object.
	UpdateRef {
		/// The ref name (e.g. `refs/heads/main`).
		name: String,
		/// The new value (a revision).
		value: String,
	},
	/// Read or set a symbolic ref.
	SymbolicRef {
		/// The symbolic ref name (e.g. `HEAD`).
		name: String,
		/// If given, set the ref to this target instead of reading it.
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
	/// Merge a commit into the current branch (fast-forward or a true merge commit).
	Merge {
		/// The commit (or branch) to merge into the current branch.
		commit: String,
		/// Merge commit message.
		#[arg(short = 'm', long = "message")]
		message: Option<String>,
		/// Always create a merge commit, even when a fast-forward is possible.
		#[arg(long = "no-ff")]
		no_ff: bool,
		/// Refuse to merge unless a fast-forward is possible.
		#[arg(long = "ff-only")]
		ff_only: bool,
	},
	/// Show the commit history of HEAD (one line per commit).
	Log,
	/// Show an object: a commit and its diff, a tag, a tree, or a blob (default: HEAD).
	Show {
		/// The object to show (default: HEAD).
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
		#[arg(short = 'l', long)]
		list: bool,
		/// Interpret the read value as a boolean.
		#[arg(long = "bool")]
		as_bool: bool,
		/// Interpret the read value as an integer.
		#[arg(long = "int")]
		as_int: bool,
		/// The dotted key (`section[.subsection].name`).
		name: Option<String>,
		/// The value to set or add.
		value: Option<String>,
	},
	/// List branches, or create one.
	Branch {
		/// Branch to create. With none, list branches.
		name: Option<String>,
		/// Revision the new branch points at (default: `HEAD`).
		start: Option<String>,
	},
	/// List tags, or create a lightweight one.
	Tag {
		/// Tag to create. With none, list tags.
		name: Option<String>,
		/// Revision the new tag points at (default: `HEAD`).
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
		name: String,
		/// Start point for `-c` (default: `HEAD`).
		start: Option<String>,
	},
	/// Switch branches, or restore working-tree files from a tree-ish or the index.
	Checkout {
		/// Discard local changes that would be overwritten.
		#[arg(short = 'f', long = "force")]
		force: bool,
		/// Branch to switch to, or tree-ish to restore paths from (before `--`).
		target: Option<String>,
		/// Paths to restore (after `--`).
		#[arg(last = true, value_name = "path")]
		paths: Vec<String>,
	},
	/// Restore working-tree and/or staged paths from a tree-ish, the index, or `HEAD`.
	Restore {
		/// Restore the working tree (the default when neither target is given).
		#[arg(short = 'W', long)]
		worktree: bool,
		/// Restore the index (staging area).
		#[arg(short = 'S', long)]
		staged: bool,
		/// Tree-ish to restore from (default: the index, or `HEAD` with `--staged`).
		#[arg(short = 's', long, value_name = "tree")]
		source: Option<String>,
		/// Paths to restore.
		#[arg(value_name = "pathspec")]
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
		/// Commit to reset to (before `--`), default `HEAD`.
		target: Option<String>,
		/// Paths to reset in the index (after `--`); does not move `HEAD`.
		#[arg(last = true, value_name = "path")]
		paths: Vec<String>,
	},
	/// Remove tracked files from the index and the working tree.
	Rm {
		/// Remove from the index only, keeping the working-tree file.
		#[arg(long)]
		cached: bool,
		/// Override the up-to-date safety check.
		#[arg(short = 'f', long = "force")]
		force: bool,
		/// Allow removing a tracked directory's contents recursively.
		#[arg(short = 'r')]
		recursive: bool,
		/// Show what would be removed without removing it.
		#[arg(short = 'n', long = "dry-run")]
		dry_run: bool,
		/// Paths to remove.
		#[arg(value_name = "pathspec")]
		pathspecs: Vec<String>,
	},
	/// Move or rename a tracked file or directory (filesystem move plus index update).
	Mv {
		/// Overwrite an existing destination.
		#[arg(short = 'f', long = "force")]
		force: bool,
		/// Show what would be moved without moving it.
		#[arg(short = 'n', long = "dry-run")]
		dry_run: bool,
		/// Report each rename performed.
		#[arg(short = 'v', long = "verbose")]
		verbose: bool,
		/// One or more sources followed by the destination.
		#[arg(value_name = "path", required = true)]
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
		url: String,
		/// Directory to clone into (default: the repository slug).
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
			Command::Commit { message } => commands::commit::run(&cwd, &message).await,
			Command::Merge {
				commit,
				message,
				no_ff,
				ff_only,
			} => commands::merge::run(&cwd, commit, message, no_ff, ff_only).await,
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
