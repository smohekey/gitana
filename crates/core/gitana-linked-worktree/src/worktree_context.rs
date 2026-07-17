//! The ambient inputs an operation reads a repository *through*: which repository, and whose config
//! decides the git-configurable behaviour.

use std::fmt;

use gitana_config::GitConfig;

use crate::RepositoryId;

/// A repository plus the configuration to honour when reading it.
///
/// The crate never *sources* the user's global/system config itself — reading `$HOME` is a
/// user-environment concern, and a library that inferred it would decide policy for its caller. The
/// caller resolves git's precedence stack (`system < global < local < config.worktree < command-scope`)
/// and injects the result here; `None` honours the repository-local config alone. So an embedding
/// consumer gets local-only determinism by default, while a git-faithful CLI passes the merged stack it
/// already resolves for the invoking worktree.
///
/// Identity ([`RepositoryId`]) stays a separate type: it answers *which* repository, and is compared and
/// echoed back on results. Config answers *how to read it*. Folding config into the identity would make
/// two contexts over one repository unequal, and identity is the thing results are matched against.
///
/// [`Debug`] deliberately **redacts** the config — see the hand-written impl below.
#[derive(Clone)]
pub struct WorktreeContext {
	repo: RepositoryId,
	effective: Option<GitConfig>,
}

/// Reports the repository and *whether* config was injected — never the config's contents.
///
/// A merged config stack routinely carries secrets: an `http.extraHeader` holding an `Authorization:
/// Bearer …`, or a remote URL with an embedded token. `GitConfig`'s derived `Debug` prints every value
/// verbatim (and again in each element's `raw` text), so a derived `Debug` here would spill the caller's
/// credentials into any log line, `{:?}` trace, or error chain that happens to include the context —
/// which is exactly what a context type invites, being passed to every operation. Deriving is a one-word
/// mistake with a wide blast radius, so the impl is written out.
impl fmt::Debug for WorktreeContext {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("WorktreeContext")
			.field("repo", &self.repo)
			.field(
				"effective_config",
				&self.effective.as_ref().map(|_| "<redacted>"),
			)
			.finish()
	}
}

impl WorktreeContext {
	/// A context over `repo` honouring the **repository-local config alone** — no global/system layer.
	pub fn new(repo: RepositoryId) -> Self {
		Self {
			repo,
			effective: None,
		}
	}

	/// A context over `repo` honouring `effective`, the caller's already-merged config stack.
	///
	/// The caller owns validation: git rejects a malformed startup boolean at process start (even a
	/// *shadowed* occurrence), which is a property of a git process booting, not of a library answering a
	/// query. A consumer that wants git's abort validates while resolving the stack — the CLI does, via
	/// `get_bool_validated` — and what arrives here is already-good config.
	pub fn with_effective_config(repo: RepositoryId, effective: GitConfig) -> Self {
		Self {
			repo,
			effective: Some(effective),
		}
	}

	/// The repository this context reads.
	pub fn repo(&self) -> &RepositoryId {
		&self.repo
	}

	/// The injected merged config, or `None` when honouring the repository-local config alone.
	pub fn effective_config(&self) -> Option<&GitConfig> {
		self.effective.as_ref()
	}
}
