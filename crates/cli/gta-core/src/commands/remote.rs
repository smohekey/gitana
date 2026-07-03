use std::path::Path;

use anyhow::{Result, bail};
use gitana_config::GitConfig;
use gitana_object::HashAlgorithm;
use gitana_repository::Repository;

use crate::Backend;
use crate::dispatch::{self, RepoCommand};

/// A `gta remote` operation: list the configured remotes, or add / remove / retarget one.
pub enum Action {
	/// List remote names (`verbose` also prints each fetch/push URL).
	List { verbose: bool },
	/// Add a remote named `name` for `url`, with the default fetch refspec.
	Add { name: String, url: String },
	/// Remove a remote and its remote-tracking refs.
	Remove { name: String },
	/// Rename a remote, moving its tracking refs and updating config that names it.
	Rename { old: String, new: String },
	/// Change a remote's URL.
	SetUrl { name: String, url: String },
}

/// Manage the repository's configured remotes — the `[remote "<name>"]` sections of `.git/config`.
pub async fn run(cwd: &Path, action: Action) -> Result<()> {
	dispatch::on_repo(cwd, RemoteCmd { action }).await
}

struct RemoteCmd {
	action: Action,
}

impl RepoCommand for RemoteCmd {
	async fn run<H: HashAlgorithm>(self, repo: Repository<Backend, H>) -> Result<()> {
		match self.action {
			Action::List { verbose } => list(&repo, verbose).await,
			Action::Add { name, url } => add(&repo, &name, &url).await,
			Action::Remove { name } => remove(&repo, &name).await,
			Action::Rename { old, new } => rename(&repo, &old, &new).await,
			Action::SetUrl { name, url } => set_url(&repo, &name, &url).await,
		}
	}
}

/// Print each remote's name, sorted. With `verbose`, also print its fetch and push URLs in git's
/// `<name>\t<url> (fetch)` / `(push)` form (just `<name>\t` when a remote has no `url`). The fetch
/// URL is the first `url` with `url.*.insteadOf` rewriting applied; the push destinations are every
/// `pushurl` verbatim, or — with none — every `url` with `pushInsteadOf` rewriting (falling back to
/// `insteadOf`), matching what `git remote -v` reports.
async fn list<H: HashAlgorithm>(repo: &Repository<Backend, H>, verbose: bool) -> Result<()> {
	let config = repo.read_config().await?;
	let fetch_rules = rewrite_rules(&config, "insteadOf");
	let push_rules = rewrite_rules(&config, "pushInsteadOf");

	let mut names = config.subsections("remote");
	names.sort_unstable();
	for name in names {
		if !verbose {
			println!("{name}");
			continue;
		}
		// An empty-string `url`/`pushurl` is treated as absent, as git does for `remote -v`.
		let urls: Vec<&str> = non_empty(config.get_all("remote", Some(name), "url"));
		match urls.first() {
			Some(url) => println!("{name}\t{} (fetch)", rewrite(url, &fetch_rules)),
			None => println!("{name}\t"),
		}
		// Push destinations. git applies `pushInsteadOf` only when falling back to `url` (no explicit
		// `pushurl`); an explicit `pushurl` gets plain `insteadOf` rewriting like any other URL.
		let pushurls = non_empty(config.get_all("remote", Some(name), "pushurl"));
		if pushurls.is_empty() {
			for &url in &urls {
				let rewritten = if starts_with_rule(url, &push_rules) {
					rewrite(url, &push_rules)
				} else {
					rewrite(url, &fetch_rules)
				};
				println!("{name}\t{rewritten} (push)");
			}
		} else {
			for push in pushurls {
				println!("{name}\t{} (push)", rewrite(push, &fetch_rules));
			}
		}
	}
	Ok(())
}

/// The URL-rewrite rules for `key` (`insteadOf` or `pushInsteadOf`): each `url.<base>.<key> =
/// <prefix>` as a `(prefix, base)` pair, in config file order so ties resolve to the first rule as
/// git does. A URL's longest matching prefix is replaced with its base.
fn rewrite_rules<'a>(config: &'a GitConfig, key: &str) -> Vec<(&'a str, &'a str)> {
	config
		.variables_named("url", key)
		.into_iter()
		.filter_map(|(base, prefix)| Some((prefix?, base?)))
		.collect()
}

/// Drop empty-string values (git treats an empty URL/pushurl as absent).
fn non_empty(values: Vec<&str>) -> Vec<&str> {
	values.into_iter().filter(|v| !v.is_empty()).collect()
}

/// Whether any rule's prefix is a prefix of `url`.
fn starts_with_rule(url: &str, rules: &[(&str, &str)]) -> bool {
	rules.iter().any(|(prefix, _)| url.starts_with(prefix))
}

/// Rewrite `url` by the longest matching rule prefix (replacing it with that rule's base), or return
/// it unchanged when nothing matches. On an equal-length tie the first rule in config order wins, as
/// git does.
fn rewrite(url: &str, rules: &[(&str, &str)]) -> String {
	let mut best: Option<&(&str, &str)> = None;
	for rule in rules.iter().filter(|(prefix, _)| url.starts_with(prefix)) {
		if best.is_none_or(|current| rule.0.len() > current.0.len()) {
			best = Some(rule);
		}
	}
	match best {
		Some((prefix, base)) => format!("{base}{}", &url[prefix.len()..]),
		None => url.to_owned(),
	}
}

async fn add<H: HashAlgorithm>(repo: &Repository<Backend, H>, name: &str, url: &str) -> Result<()> {
	validate_name(name)?;
	let mut config = repo.read_config().await?;
	if config.subsections("remote").contains(&name) {
		bail!("remote '{name}' already exists");
	}
	config.set("remote", Some(name), "url", url)?;
	config.set(
		"remote",
		Some(name),
		"fetch",
		&format!("+refs/heads/*:refs/remotes/{name}/*"),
	)?;
	repo.write_config(&config).await?;
	Ok(())
}

async fn set_url<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	name: &str,
	url: &str,
) -> Result<()> {
	let mut config = repo.read_config().await?;
	if !config.subsections("remote").contains(&name) {
		bail!("no such remote: '{name}'");
	}
	config.set("remote", Some(name), "url", url)?;
	repo.write_config(&config).await?;
	Ok(())
}

async fn remove<H: HashAlgorithm>(repo: &Repository<Backend, H>, name: &str) -> Result<()> {
	let mut config = repo.read_config().await?;
	// git treats an empty `[remote "x"]` header as no remote; gate on a real (variable-bearing) one.
	if !config.subsections("remote").contains(&name) {
		bail!("no such remote: '{name}'");
	}
	config.remove_subsection("remote", name);
	// Drop every branch's fetch/push config that named this remote, and a repo-level push default
	// pointing at it — everything git's `remote remove` clears so later ops don't chase a gone remote.
	let branches: Vec<String> = config
		.subsections("branch")
		.into_iter()
		.map(str::to_owned)
		.collect();
	for branch in branches {
		if config.get_string("branch", Some(&branch), "remote") == Some(name) {
			config.unset("branch", Some(&branch), "remote");
			config.unset("branch", Some(&branch), "merge");
		}
		if config.get_string("branch", Some(&branch), "pushRemote") == Some(name) {
			config.unset("branch", Some(&branch), "pushRemote");
		}
	}
	if config.get_string("remote", None, "pushDefault") == Some(name) {
		config.unset("remote", None, "pushDefault");
	}
	repo.write_config(&config).await?;

	// Delete the remote's tracking refs (`refs/remotes/<name>/*`) — direct, symbolic, and packed.
	repo
		.refs()
		.remove_prefix(&format!("refs/remotes/{name}/"))
		.await?;
	Ok(())
}

async fn rename<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	old: &str,
	new: &str,
) -> Result<()> {
	validate_name(new)?;
	let mut config = repo.read_config().await?;
	let remotes = config.subsections("remote");
	if !remotes.contains(&old) {
		bail!("no such remote: '{old}'");
	}
	if remotes.contains(&new) {
		bail!("remote '{new}' already exists");
	}
	let (old_tracking, new_tracking) = (
		format!("refs/remotes/{old}/"),
		format!("refs/remotes/{new}/"),
	);
	config.rename_subsection("remote", old, new);

	// Point the *destination* of each fetch refspec at the new tracking-ref namespace. Only the
	// `<src>:<dst>` right-hand side is the tracking namespace; git leaves the left (source) side —
	// what to fetch from the remote — alone, so we must not rewrite a source `refs/remotes/<old>/`.
	let fetches: Vec<String> = config
		.get_all("remote", Some(new), "fetch")
		.iter()
		.map(|refspec| match refspec.split_once(':') {
			// Only a destination that *is* the old tracking namespace is repointed — git leaves a
			// destination that merely contains it as a substring (e.g. `xrefs/remotes/old/*`) alone.
			Some((src, dst)) if dst.starts_with(&old_tracking) => {
				format!("{src}:{new_tracking}{}", &dst[old_tracking.len()..])
			}
			_ => (*refspec).to_owned(),
		})
		.collect();
	config.unset("remote", Some(new), "fetch");
	for refspec in &fetches {
		config.add("remote", Some(new), "fetch", Some(refspec));
	}

	// Repoint any branch upstream/push config and the repo push default at the new name.
	let branches: Vec<String> = config
		.subsections("branch")
		.into_iter()
		.map(str::to_owned)
		.collect();
	for branch in branches {
		if config.get_string("branch", Some(&branch), "remote") == Some(old) {
			config.set("branch", Some(&branch), "remote", new)?;
		}
		if config.get_string("branch", Some(&branch), "pushRemote") == Some(old) {
			config.set("branch", Some(&branch), "pushRemote", new)?;
		}
	}
	if config.get_string("remote", None, "pushDefault") == Some(old) {
		config.set("remote", None, "pushDefault", new)?;
	}
	repo.write_config(&config).await?;

	// Move the tracking refs (and reflogs) to the new namespace.
	repo
		.refs()
		.rename_prefix(&old_tracking, &new_tracking)
		.await?;
	Ok(())
}

/// Reject a remote name that would produce an invalid `refs/remotes/<name>/*` refspec, applying
/// git's refname rules to the `<name>` path segments. `<name>` is always a *middle* segment (the
/// branch is the last), so the whole-refname-only rules (a trailing `.`, the single `@`) do not
/// apply — git accepts remotes like `@` and `foo.`. A bad name would otherwise silently write config
/// that stock `git fetch`/`git remote -v` then rejects.
fn validate_name(name: &str) -> Result<()> {
	// Anywhere in the name: no `..` or `@{`, and no ASCII control, space, or refname-special /
	// config-breaking byte.
	let anywhere_bad = name.contains("..")
		|| name.contains("@{")
		|| name.chars().any(|c| {
			// git's refname-invalid bytes. A `"` is allowed: it is valid in a refname and the config
			// writer escapes it in the `[remote "…"]` subsection header.
			c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
		});
	// Each `/`-separated segment (empty catches a leading/trailing/double slash) must be a valid
	// refname component: non-empty, not starting with `.`, not ending with `.lock`.
	let segment_bad = name
		.split('/')
		.any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"));
	if name.is_empty() || anywhere_bad || segment_bad {
		bail!("invalid remote name: '{name}'");
	}
	Ok(())
}
