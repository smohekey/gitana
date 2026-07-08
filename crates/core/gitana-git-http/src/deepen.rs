//! A client's history-deepening directive for a shallow fetch.

/// The `deepen*` lines a client sends in an upload-pack request to bound the history it wants.
///
/// Mirrors git's `--depth` / `--shallow-since` / `--shallow-exclude`. An all-empty `Deepen` (the
/// [`Default`]) emits no lines, so a normal (non-shallow) fetch request is unchanged. `depth` and
/// `since` are independent wire directives; the CLI enforces git's mutual exclusions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deepen {
	/// `deepen <n>`: keep only `n` commits from each want tip (depth 1 = the tips alone).
	pub depth: Option<u32>,
	/// `deepen-since <t>`: keep commits with a committer time at or after this Unix timestamp.
	pub since: Option<i64>,
	/// `deepen-not <ref>`: stop deepening at these refs/oids (and their ancestors). Repeatable.
	pub not: Vec<String>,
}

impl Deepen {
	/// Whether this directive requests no deepening at all (emits no `deepen*` lines).
	pub fn is_empty(&self) -> bool {
		self.depth.is_none() && self.since.is_none() && self.not.is_empty()
	}
}
