use anyhow::Result;

/// Resolves the git identity lines (`Name <email> seconds ±hhmm`) for operations that record commits
/// or reflog entries. The engine never reads process env / config directly; a frontend implements
/// this over its resolution. Methods are async so resolution stays lazy — an operation asks only once
/// it is certain to record something (e.g. after the empty-index guard of a commit).
pub trait Identity {
	/// The author line; errors if no identity is configured.
	async fn author(&self) -> Result<String>;
	/// The committer line; errors if no identity is configured.
	async fn committer(&self) -> Result<String>;
	/// The committer line for a reflog entry, defaulting a *missing* identity to a placeholder rather
	/// than failing — git records these (fast-forward, abort) without a configured identity. It still
	/// errors if the configuration itself cannot be loaded, so a malformed config aborts the operation.
	async fn committer_or_default(&self) -> Result<String>;
}
