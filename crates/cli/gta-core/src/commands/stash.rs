//! `gta stash`: push, apply, pop, list, show (file names) and drop, as git's, on gitana's stash.
//! Each prints git's lines (measured against git 2.55); after apply and pop git also prints the
//! long status, which gta leaves out.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use gitana_object::{HashAlgorithm, ObjectKind};
use gitana_path::{GitPath, GitPathspec};
use gitana_worktree::{
	StashApplied, StashApply, StashApplyError, StashPopError, StashPush, StashPushError, WorkTree,
	WorktreeError,
};

use crate::dispatch::{self, WorkTreeCommand};
use crate::identity::CliIdentity;
use crate::{Backend, ResultPathMode, SilentExit};

/// A `stash` sub-command.
pub enum Action {
	/// `stash [push] [-u] [-m <message>] [-q] [--] [<pathspec>…]`.
	Push {
		pathspecs: Vec<GitPathspec>,
		message: Option<String>,
		include_untracked: bool,
		quiet: bool,
	},
	/// `stash apply [--index] [-q] [<stash>]`.
	Apply {
		stash: Option<String>,
		index: bool,
		quiet: bool,
	},
	/// `stash pop [--index] [-q] [<stash>]`.
	Pop {
		stash: Option<String>,
		index: bool,
		quiet: bool,
	},
	/// `stash list`.
	List,
	/// `stash show --name-only [--include-untracked] [<stash>]`.
	Show {
		stash: Option<String>,
		include_untracked: bool,
	},
	/// `stash drop [-q] [<stash>]`.
	Drop { stash: Option<String>, quiet: bool },
}

pub async fn run(cwd: &Path, action: Action, result_path_mode: ResultPathMode) -> Result<()> {
	let command = Stash {
		action,
		cwd: cwd.to_path_buf(),
		result_path_mode,
	};
	match command.action {
		Action::List | Action::Show { .. } => dispatch::on_worktree(cwd, command).await,
		_ => dispatch::on_worktree_history_mutation(cwd, command).await,
	}
}

struct Stash {
	action: Action,
	cwd: PathBuf,
	result_path_mode: ResultPathMode,
}

/// A `<stash>` as git takes it: none (the newest, spelled `refs/stash@{0}`), `N` (`refs/stash@{N}`),
/// or `stash@{N}` as written.
struct Which {
	index: usize,
	/// How git names it in "Dropped …".
	spelled: String,
	/// The ref git names when there's no such entry ("log for '…' only has …").
	log: &'static str,
}

/// The stash `stash` names, or git's refusal for a spelling that names none (worded as git words it,
/// by whether any stash exists).
async fn which<F: gitana_file_store::FileStore, H: HashAlgorithm>(
	repo: &gitana_repository::Repository<F, H>,
	stash: Option<&str>,
) -> Result<Which> {
	if let Some(which) = parse_which(stash) {
		return Ok(which);
	}
	let stash = stash.unwrap_or_default();
	Err(if repo.stash_list().await?.is_empty() {
		fail(&[format!("error: {stash} is not a valid reference")], &[])
	} else {
		fail(&[format!("error: not a reflog: {stash}")], &[])
	})
}

/// [`which`]'s spellings: none, `N` or `stash@{N}`.
fn parse_which(stash: Option<&str>) -> Option<Which> {
	let Some(stash) = stash else {
		return Some(Which {
			index: 0,
			spelled: "refs/stash@{0}".to_owned(),
			log: "refs/stash",
		});
	};
	if let Ok(index) = stash.parse::<usize>() {
		return Some(Which {
			index,
			spelled: format!("refs/stash@{{{index}}}"),
			log: "refs/stash",
		});
	}
	let index = stash
		.strip_prefix("stash@{")
		.and_then(|rest| rest.strip_suffix('}'))
		.and_then(|index| index.parse::<usize>().ok())?;
	Some(Which {
		index,
		spelled: stash.to_owned(),
		log: "stash",
	})
}

/// `path` as git's `quote_path` shows it from `prefix`, the current folder: a `../` for each of
/// the prefix's folders it isn't under.
fn render_from(prefix: &GitPath, path: &GitPath, mode: ResultPathMode) -> String {
	let ours: Vec<&[u8]> = prefix
		.components()
		.filter(|part| !part.is_empty())
		.collect();
	let theirs: Vec<&[u8]> = path.components().filter(|part| !part.is_empty()).collect();
	let common = ours.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
	let ups = "../".repeat(ours.len() - common);
	let Ok(rest) = GitPath::from_bytes(theirs[common..].join(&b'/')) else {
		return format!("{ups}{}", crate::git_path::render_result_path(path, mode));
	};
	match mode {
		ResultPathMode::Human => format!("{ups}{rest}"),
		ResultPathMode::Reversible => rest.quote_with_affixes(ups.as_bytes(), b""),
	}
}

/// Print `errors` to stderr and `then` to stdout, as git does, then git's non-zero exit.
fn fail(errors: &[String], then: &[String]) -> anyhow::Error {
	for line in errors {
		eprintln!("{line}");
	}
	for line in then {
		println!("{line}");
	}
	SilentExit {
		reason: "stash refused",
	}
	.into()
}

const KEPT: &str = "The stash entry is kept in case you need it again.";

impl WorkTreeCommand for Stash {
	/// The directory the command runs in once the repository is held: the work tree's top when run
	/// from where `.git` is while `core.worktree` puts the work tree elsewhere, as git changes into it,
	/// so a relative excludes file is found there.
	fn set_command_directory(&mut self, path: std::path::PathBuf, _directory: cap_std::fs::Dir) {
		self.cwd = path;
	}

	async fn run<H: HashAlgorithm>(
		self,
		worktree: WorkTree<Backend, crate::WorkDir, H>,
		prefix: GitPath,
	) -> Result<()> {
		let mode = self.result_path_mode;
		let render = |path: &GitPath| crate::git_path::render_result_path(path, mode);
		let repo = worktree.repository();
		let pop = matches!(self.action, Action::Pop { .. });
		match self.action {
			Action::Push {
				pathspecs,
				message,
				include_untracked,
				quiet,
			} => {
				let config = repo.effective_config().await?;
				let excludes_file =
					crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?;
				let request = StashPush {
					pathspecs,
					prefix: prefix.clone(),
					message,
					include_untracked,
					excludes_file,
				};
				let identity = CliIdentity::new(repo);
				// Stored, though for `Clean` with untracked files left (git: "Saved …", a warning for
				// each, then a failure).
				let stored = match worktree.stash_push(&request, &identity).await {
					Ok(None) => Ok(None),
					Ok(Some(pushed)) => Ok(Some((pushed, Vec::new()))),
					Err(StashPushError::Clean {
						pushed,
						not_removed,
					}) => Ok(Some((*pushed, not_removed))),
					Err(other) => Err(other),
				};
				match stored {
					Ok(None) => {
						if !quiet {
							println!("No local changes to save");
						}
						Ok(())
					}
					Ok(Some((pushed, not_removed))) => {
						for path in &pushed.skipped_repositories {
							eprintln!("Ignoring path {}/", render(path));
						}
						for (path, change) in &pushed.line_ending_warnings {
							let (from, to) = match change {
								gitana_worktree::LineEndingChange::CrlfToLf => ("CRLF", "LF"),
								gitana_worktree::LineEndingChange::LfToCrlf => ("LF", "CRLF"),
							};
							eprintln!(
								"warning: in the working copy of '{}', {from} will be replaced by {to} the next time Git touches it",
								render(path)
							);
						}
						if !quiet {
							let (kind, payload) = repo.objects().read_object(&pushed.entry.commit).await?;
							let message = match kind {
								ObjectKind::Commit => gitana_object::decode_commit::<H>(&payload)?.commit.message,
								_ => pushed.entry.message.clone(),
							};
							println!("Saved working directory and index state {message}");
						}
						if !not_removed.is_empty() {
							for (path, error) in &not_removed {
								eprintln!(
									"warning: failed to remove {}: {}",
									render_from(&prefix, path, mode),
									crate::attributes::strerror(error)
								);
							}
							return Err(fail(&[], &[]));
						}
						// Put away, but git's clean-up leaves what lies outside the sparse checkout,
						// with its advisory (whatever `-q`), and fails.
						if !pushed.sparse_pathspecs.is_empty() {
							let show_hints = config
								.get_bool("advice", None, "updateSparsePath")?
								.unwrap_or(true);
							let sparse = pushed
								.sparse_pathspecs
								.iter()
								.map(|spec| pathspec_origin(&prefix, spec, mode))
								.collect();
							return Err(crate::AddAdvisory::new(sparse, Vec::new(), show_hints, false).into());
						}
						Ok(())
					}
					Err(StashPushError::NoInitialCommit) => Err(fail(
						&["You do not have the initial commit yet".to_owned()],
						&[],
					)),
					Err(StashPushError::Unmerged(paths)) => {
						for path in &paths {
							println!("{}: needs merge", render(path));
						}
						Err(fail(&["error: could not write index".to_owned()], &[]))
					}
					Err(StashPushError::UnmatchedPathspec(spec)) => Err(fail(
						&[
							format!(
								"error: pathspec '{}' did not match any file(s) known to git",
								pathspec_origin(&prefix, &spec, mode)
							),
							"Did you forget to 'git add'?".to_owned(),
						],
						&[],
					)),
					Err(StashPushError::IntentToAdd(paths)) => Err(fail(
						&[
							format!(
								"error: Entry '{}' not uptodate. Cannot merge.",
								paths.first().map(render).unwrap_or_default()
							),
							"Cannot save the current worktree state".to_owned(),
						],
						&[],
					)),
					// Unwrapped, so the error's own rendering of its paths is used.
					Err(StashPushError::Reset(error)) => Err(error.into()),
					Err(StashPushError::Worktree(error)) => Err(error.into()),
					Err(StashPushError::Repository(error)) => Err(error.into()),
					Err(other) => Err(other.into()),
				}
			}
			Action::Apply {
				stash,
				index,
				quiet,
			}
			| Action::Pop {
				stash,
				index,
				quiet,
			} => {
				let which = which(repo, stash.as_deref()).await?;
				let config = repo.effective_config().await?;
				let excludes_file =
					crate::excludes::resolve_excludes_file(&config, &self.cwd, &prefix).await?;
				let request = StashApply {
					index: which.index,
					restore_index: index,
					excludes_file,
				};
				let up_to_date = holds_no_tracked_change(repo, which.index).await?;
				let outcome = if pop {
					worktree.stash_pop(&request).await
				} else {
					worktree
						.stash_apply(&request)
						.await
						.map_err(StashPopError::Apply)
				};
				// git's merge says so, even with `-q`, when the stash changes no tracked file; it runs
				// after every refusal but the untracked clash.
				if up_to_date
					&& matches!(
						outcome,
						Ok(_) | Err(StashPopError::Apply(StashApplyError::UntrackedExists(_)))
					) {
					println!("Already up to date.");
				}
				match outcome {
					Ok(applied) => {
						for (path, aside) in &applied.moved_aside {
							println!("WARNING: Untracked file in way of tracked file!  Renaming");
							println!("            {} -> {}", render(path), render(aside));
							println!("         to make room.");
						}
						for path in &applied.replaced {
							eprintln!(
								"error: '{}' appears as both a file and as a directory",
								render(path)
							);
						}
						report_applied(&applied, index, &render);
						if applied.conflicts.is_empty() {
							if pop && !quiet {
								println!("Dropped {} ({})", which.spelled, applied.commit);
							}
							Ok(())
						} else {
							let kept = if pop {
								vec![KEPT.to_owned()]
							} else {
								Vec::new()
							};
							Err(fail(&[], &kept))
						}
					}
					Err(StashPopError::Drop { error, .. }) => Err(error.into()),
					// Unwrapped, so the error's own rendering of its paths is used.
					Err(StashPopError::Apply(StashApplyError::Worktree(error)))
						if !matches!(
							error,
							WorktreeError::Conflict(_) | WorktreeError::ConflictOverwrite(_)
						) =>
					{
						Err(error.into())
					}
					Err(StashPopError::Apply(StashApplyError::Repository(error))) => Err(error.into()),
					Err(StashPopError::Apply(error)) => {
						let lines = apply_refusal(&error, &which, &render)?;
						let kept = if pop
							&& !matches!(
								error,
								StashApplyError::NoEntries | StashApplyError::OutOfRange { .. }
							) {
							vec![KEPT.to_owned()]
						} else {
							Vec::new()
						};
						Err(fail(&lines, &kept))
					}
				}
			}
			Action::List => {
				for entry in repo.stash_list().await? {
					println!("stash@{{{}}}: {}", entry.index, entry.message);
				}
				Ok(())
			}
			Action::Show {
				stash,
				include_untracked,
			} => {
				let which = which(repo, stash.as_deref()).await?;
				let Some(files) = repo.stash_files(which.index, include_untracked).await? else {
					let count = repo.stash_list().await?.len();
					return Err(if count == 0 {
						fail(&["No stash entries found.".to_owned()], &[])
					} else {
						fail(
							&[format!(
								"fatal: log for '{}' only has {count} entries",
								which.log
							)],
							&[],
						)
					});
				};
				for path in &files {
					println!("{}", render(path));
				}
				Ok(())
			}
			Action::Drop { stash, quiet } => {
				let which = which(repo, stash.as_deref()).await?;
				match repo.drop_stash(which.index, None).await? {
					Some(commit) => {
						if !quiet {
							println!("Dropped {} ({commit})", which.spelled);
						}
						Ok(())
					}
					None => {
						let count = repo.stash_list().await?.len();
						Err(if count == 0 {
							fail(&["No stash entries found.".to_owned()], &[])
						} else {
							fail(
								&[format!(
									"fatal: log for '{}' only has {count} entries",
									which.log
								)],
								&[],
							)
						})
					}
				}
			}
		}
	}
}

/// A pathspec as git spells it in stash's messages (its `PATHSPEC_PREFIX_ORIGIN` form): the magic
/// (a long form as written, a short one by name), the length of the prefix left once `..` is
/// resolved, then the path from the top, a trailing `/` kept. `../d` from `e/` is `:(prefix:0)d`;
/// `:/d` is `:(top,prefix:0)d`; `:(glob)d/*` is `:(glob,prefix:0)d/*` (measured against git 2.55).
/// Rendered as the front end renders pathspecs (gta-mcp's form keeps bytes that aren't UTF-8
/// recoverable).
fn pathspec_origin(prefix: &GitPath, spec: &GitPathspec, mode: ResultPathMode) -> String {
	let bytes = spec.as_bytes();
	let (magic, rest, top): (Vec<u8>, &[u8], bool) = if let Some(long) = bytes.strip_prefix(b":(")
		&& let Some(close) = long.iter().position(|&byte| byte == b')')
	{
		let words = &long[..close];
		let top = words.split(|&byte| byte == b',').any(|word| word == b"top");
		(bytes[..2 + close].to_vec(), &long[close + 1..], top)
	} else if let Some(short) = bytes.strip_prefix(b":") {
		let count = short
			.iter()
			.take_while(|byte| matches!(byte, b'/' | b'!' | b'^'))
			.count();
		let marks = &short[..count];
		let rest = &short[count..];
		let rest = rest.strip_prefix(b":").unwrap_or(rest);
		let top = marks.contains(&b'/');
		let mut names = Vec::new();
		if top {
			names.push("top");
		}
		if marks.iter().any(|byte| matches!(byte, b'!' | b'^')) {
			names.push("exclude");
		}
		(format!(":({}", names.join(",")).into_bytes(), rest, top)
	} else {
		(b":(".to_vec(), bytes, false)
	};
	let mut parts: Vec<&[u8]> = if top || prefix.is_root() {
		Vec::new()
	} else {
		prefix.as_bytes().split(|&byte| byte == b'/').collect()
	};
	let mut kept = parts.len();
	for part in rest.split(|&byte| byte == b'/') {
		match part {
			b"" | b"." => {}
			b".." => {
				parts.pop();
				kept = kept.min(parts.len());
			}
			part => parts.push(part),
		}
	}
	let prefix_len: usize = parts[..kept].iter().map(|part| part.len() + 1).sum();
	let mut out = magic;
	if out.len() > 2 {
		out.push(b',');
	}
	out.extend_from_slice(format!("prefix:{prefix_len})").as_bytes());
	out.extend_from_slice(&parts.join(&b'/'));
	if rest.ends_with(b"/") && !parts.is_empty() {
		out.push(b'/');
	}
	match GitPathspec::from_bytes(out) {
		Ok(origin) => crate::git_path::render_result_pathspec(&origin, mode),
		Err(error) => error.to_string(),
	}
}

/// Whether `stash@{index}` changes no tracked file (its tree is its base's), when there is one.
async fn holds_no_tracked_change<F: gitana_file_store::FileStore, H: HashAlgorithm>(
	repo: &gitana_repository::Repository<F, H>,
	index: usize,
) -> Result<bool> {
	let Some(entry) = repo
		.stash_list()
		.await?
		.into_iter()
		.find(|entry| entry.index == index)
	else {
		return Ok(false);
	};
	let commit = read_links(repo, entry.commit).await?;
	let Some(base) = commit.parents.first() else {
		return Ok(false);
	};
	Ok(read_links(repo, *base).await?.tree == commit.tree)
}

async fn read_links<F: gitana_file_store::FileStore, H: HashAlgorithm>(
	repo: &gitana_repository::Repository<F, H>,
	id: gitana_object::ObjectId<H>,
) -> Result<gitana_object::CommitLinks<H>> {
	let (kind, payload) = repo.objects().read_object(&id).await?;
	if kind != ObjectKind::Commit {
		return Err(anyhow!("{id} is not a commit"));
	}
	Ok(gitana_object::commit_links::<H>(&payload)?)
}

/// git's lines for a conflicted apply.
fn report_applied<H: HashAlgorithm>(
	applied: &StashApplied<H>,
	restore_index: bool,
	render: &impl Fn(&GitPath) -> String,
) {
	if applied.conflicts.is_empty() {
		return;
	}
	for path in &applied.conflicts {
		println!("Auto-merging {}", render(path));
		println!("CONFLICT (content): Merge conflict in {}", render(path));
	}
	if restore_index {
		eprintln!("Index was not unstashed.");
	}
}

/// git's lines for an apply that brought nothing back.
fn apply_refusal(
	error: &StashApplyError,
	which: &Which,
	render: &impl Fn(&GitPath) -> String,
) -> Result<Vec<String>> {
	Ok(match error {
		StashApplyError::NoEntries => vec!["No stash entries found.".to_owned()],
		StashApplyError::OutOfRange { count, .. } => vec![format!(
			"fatal: log for '{}' only has {count} entries",
			which.log
		)],
		StashApplyError::NotStashLike(commit) => {
			vec![format!("error: '{commit}' is not a stash-like commit")]
		}
		StashApplyError::Unmerged => {
			vec!["error: cannot apply a stash in the middle of a merge".to_owned()]
		}
		StashApplyError::IndexConflicts => {
			vec!["error: conflicts in index. Try without --index.".to_owned()]
		}
		StashApplyError::UntrackedExists(path) => vec![
			format!("{} already exists, no checkout", render(path)),
			"error: could not restore untracked files from stash".to_owned(),
		],
		StashApplyError::Worktree(WorktreeError::Conflict(path)) => vec![
			"error: Your local changes to the following files would be overwritten by merge:".to_owned(),
			format!("\t{}", render(path)),
			"Please commit your changes or stash them before you merge.".to_owned(),
			"Aborting".to_owned(),
		],
		StashApplyError::Worktree(WorktreeError::ConflictOverwrite(refusal)) => {
			vec![format!("error: {}", refusal.render_with_paths(render))]
		}
		other => return Err(anyhow!("{other}")),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// git's spelling, kept whole for gta-mcp: a name that isn't UTF-8 stays distinct from the
	/// UTF-8 name a lossy rendering would make of it, and a plain one reads as git's.
	#[test]
	fn pathspec_origins_keep_their_bytes_where_the_front_end_does() {
		let prefix = GitPath::from_utf8("e").unwrap();
		let raw = GitPathspec::from_bytes(b"../raw-\xff".to_vec()).unwrap();
		let lossy = GitPathspec::from_utf8("../raw-\u{fffd}").unwrap();
		let plain = GitPathspec::from_utf8(":(glob)../d/*").unwrap();
		let reversible = |spec| pathspec_origin(&prefix, spec, ResultPathMode::Reversible);
		assert_ne!(reversible(&raw), reversible(&lossy));
		assert_eq!(reversible(&plain), ":(glob,prefix:0)d/*");
		assert_eq!(
			pathspec_origin(&prefix, &plain, ResultPathMode::Human),
			":(glob,prefix:0)d/*"
		);
	}
}
