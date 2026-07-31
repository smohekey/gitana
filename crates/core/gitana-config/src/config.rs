use crate::{ConfigError, GitConfigSource};

/// A layered git configuration: an ordered stack of [`GitConfigSource`] files resolved with git's
/// precedence, plus one designated *writable* source that edits are directed at.
///
/// Sources are held in git's **read order** — lowest precedence first (system, then global, then
/// the repository-local file). A single-valued lookup returns the value from the highest-precedence
/// file that sets the key (git's last-writer-wins); a multi-valued lookup concatenates every file's
/// values in read order. Writes, [`render`](Self::render), and the deletion/rename helpers act on the
/// writable source alone, so editing the effective config never disturbs a lower layer.
///
/// The common case — a config parsed from a single file (`parse`, `new`) — is a one-source stack whose
/// sole source is writable, so it behaves exactly like the underlying [`GitConfigSource`].
#[derive(Debug, Clone)]
pub struct GitConfig {
	/// Sources in read order: lowest precedence first, highest last.
	sources: Vec<GitConfigSource>,
	/// Index into `sources` that writes and `render` target.
	writable: usize,
}

impl Default for GitConfig {
	fn default() -> Self {
		Self::new()
	}
}

impl GitConfig {
	/// An empty config: a single writable source with no variables.
	pub fn new() -> Self {
		Self::single(GitConfigSource::new())
	}

	/// Parse a single config file's text into a one-source (writable) config.
	pub fn parse(text: &str) -> Result<Self, ConfigError> {
		Ok(Self::single(GitConfigSource::parse(text)?))
	}

	/// A one-source config whose sole source is writable.
	pub fn single(source: GitConfigSource) -> Self {
		Self {
			sources: vec![source],
			writable: 0,
		}
	}

	/// Build a layered config from sources in read order (lowest precedence first). The last
	/// (highest-precedence) source is writable. Panics on an empty stack — a config always has at
	/// least the file writes land in.
	pub fn from_sources(sources: Vec<GitConfigSource>) -> Self {
		assert!(
			!sources.is_empty(),
			"a GitConfig needs at least one source (the writable one)"
		);
		let writable = sources.len() - 1;
		Self { sources, writable }
	}

	/// Like [`from_sources`](Self::from_sources), but an empty stack yields an empty config (a single
	/// writable source) instead of panicking — for reading a scope whose files are all absent.
	pub fn from_sources_or_empty(sources: Vec<GitConfigSource>) -> Self {
		if sources.is_empty() {
			Self::new()
		} else {
			Self::from_sources(sources)
		}
	}

	/// Layer `lower` beneath the existing stack as lower-precedence sources (in read order — earliest
	/// first), keeping the current writable source. Used CLI-side to slip global and system files
	/// under a repository-local config, so lookups gain git's full precedence while writes still land
	/// in the local file.
	pub fn underlay(&mut self, lower: impl IntoIterator<Item = GitConfigSource>) {
		let mut combined: Vec<GitConfigSource> = lower.into_iter().collect();
		let added = combined.len();
		combined.append(&mut self.sources);
		self.sources = combined;
		self.writable += added;
	}

	/// Layer `higher` above the existing stack as higher-precedence sources (in read order — lowest of
	/// the added first), keeping the current writable source. Used to place environment config
	/// (git's `GIT_CONFIG_COUNT` / `-c` entries) above the repository-local file for a merged read,
	/// while writes still land in the local file below.
	pub fn overlay(&mut self, higher: impl IntoIterator<Item = GitConfigSource>) {
		self.sources.extend(higher);
	}

	/// The single-valued lookup: the value from the highest-precedence source that sets the key
	/// (git's last-writer-wins), or `None` if unset (or set only as boolean-true).
	pub fn get_string(&self, section: &str, subsection: Option<&str>, name: &str) -> Option<&str> {
		self.get_raw(section, subsection, name).flatten()
	}

	/// Every value for a multi-valued variable, concatenated across the sources in read order.
	pub fn get_all(&self, section: &str, subsection: Option<&str>, name: &str) -> Vec<&str> {
		self
			.sources
			.iter()
			.flat_map(|s| s.get_all(section, subsection, name))
			.collect()
	}

	/// Like [`get_string`](Self::get_string) but keeps "absent" distinct from "present but valueless":
	/// outer `None` if no source sets the key, inner `None` for a bare (valueless) variable. Resolved
	/// against the highest-precedence source that sets it.
	pub fn get_raw(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Option<Option<&str>> {
		self
			.sources
			.iter()
			.rev()
			.find_map(|source| source.get_raw(section, subsection, name))
	}

	/// Every matching value across the sources in read order (inner `None` for a valueless one);
	/// empty if unset anywhere.
	pub fn get_all_raw(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Vec<Option<&str>> {
		self
			.sources
			.iter()
			.flat_map(|s| s.get_all_raw(section, subsection, name))
			.collect()
	}

	/// Interpret the effective (highest-precedence) value as a git boolean. `None` if unset anywhere.
	pub fn get_bool(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Result<Option<bool>, ConfigError> {
		match self.winning_source(section, subsection, name) {
			Some(source) => source.get_bool(section, subsection, name),
			None => Ok(None),
		}
	}

	/// Interpret the effective (highest-precedence) value as a git boolean, **validating every occurrence**
	/// across all sources — not just the winning one. git reads its `core.*` startup booleans (e.g.
	/// `core.ignorecase`, `core.bare`) eagerly and aborts on *any* malformed value, even one a
	/// higher-precedence source shadows; this reproduces that. Use it for a boolean git parses at startup;
	/// the lazy [`get_bool`](Self::get_bool) (validating only the winning value) matches git's on-demand
	/// reads of a config it only consults when needed.
	pub fn get_bool_validated(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Result<Option<bool>, ConfigError> {
		// Every value in read order (lowest precedence first): validate each, so a malformed shadowed value
		// still errors, and keep the last (highest-precedence) as the effective result.
		let mut effective = None;
		for raw in self.get_all_raw(section, subsection, name) {
			effective = Some(crate::source::interpret_bool(raw)?);
		}
		Ok(effective)
	}

	/// Interpret the effective (highest-precedence) value as a git integer. `None` if unset anywhere.
	pub fn get_int(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Result<Option<i64>, ConfigError> {
		match self.winning_source(section, subsection, name) {
			Some(source) => source.get_int(section, subsection, name),
			None => Ok(None),
		}
	}

	/// Interpret the effective (highest-precedence) value as a git integer, **validating every occurrence**
	/// across all sources — the integer counterpart of [`get_bool_validated`](Self::get_bool_validated). git
	/// reads its startup integers (e.g. `core.repositoryformatversion`) eagerly and aborts on *any* malformed
	/// value, even one a higher-precedence occurrence shadows; this reproduces that. `None` if unset anywhere.
	pub fn get_int_validated(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Result<Option<i64>, ConfigError> {
		// Every value in read order (lowest precedence first): validate each, so a malformed shadowed value
		// still errors, and keep the last (highest-precedence) as the effective result.
		let mut effective = None;
		for raw in self.get_all_raw(section, subsection, name) {
			effective = Some(crate::source::interpret_int(raw.unwrap_or(""))?);
		}
		Ok(effective)
	}

	/// The distinct subsection names with at least one variable under `section`, in first-seen order
	/// across the sources in read order.
	pub fn subsections(&self, section: &str) -> Vec<&str> {
		let mut names: Vec<&str> = Vec::new();
		for source in &self.sources {
			for name in source.subsections(section) {
				if !names.contains(&name) {
					names.push(name);
				}
			}
		}
		names
	}

	/// Every variable named `name` under `section` across all subsections, in read order across the
	/// sources, as `(subsection, value)` pairs (see [`GitConfigSource::variables_named`]).
	pub fn variables_named<'a>(
		&'a self,
		section: &str,
		name: &str,
	) -> Vec<(Option<&'a str>, Option<&'a str>)> {
		self
			.sources
			.iter()
			.flat_map(|s| s.variables_named(section, name))
			.collect()
	}

	/// Every variable as a dotted key and value, in read order across the sources — a lower layer's
	/// entries first, then the layers above it. For `--list`, which prints the merged config.
	pub fn entries(&self) -> Vec<(String, Option<&str>)> {
		self.sources.iter().flat_map(|s| s.entries()).collect()
	}

	/// Set a variable in the writable source (see [`GitConfigSource::set`]).
	pub fn set(
		&mut self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
		value: &str,
	) -> Result<(), ConfigError> {
		self.writable_mut().set(section, subsection, name, value)
	}

	/// Replace every value of a variable in the writable source (see
	/// [`GitConfigSource::replace_all`]).
	pub fn replace_all(&mut self, section: &str, subsection: Option<&str>, name: &str, value: &str) {
		self
			.writable_mut()
			.replace_all(section, subsection, name, value)
	}

	/// Append a value in the writable source (see [`GitConfigSource::add`]).
	pub fn add(&mut self, section: &str, subsection: Option<&str>, name: &str, value: Option<&str>) {
		self.writable_mut().add(section, subsection, name, value)
	}

	/// Remove every value of a variable from the writable source (see [`GitConfigSource::unset`]).
	pub fn unset(&mut self, section: &str, subsection: Option<&str>, name: &str) -> bool {
		self.writable_mut().unset(section, subsection, name)
	}

	/// Remove a whole subsection from the writable source (see
	/// [`GitConfigSource::remove_subsection`]).
	pub fn remove_subsection(&mut self, section: &str, subsection: &str) -> bool {
		self.writable_mut().remove_subsection(section, subsection)
	}

	/// Rename a subsection in the writable source (see [`GitConfigSource::rename_subsection`]).
	pub fn rename_subsection(&mut self, section: &str, old: &str, new: &str) -> bool {
		self.writable_mut().rename_subsection(section, old, new)
	}

	/// Serialise the writable source back to git config text (see [`GitConfigSource::render`]). Only
	/// the writable layer is rendered — a layered config is never written back as one file.
	pub fn render(&self) -> String {
		self.sources[self.writable].render()
	}

	/// The highest-precedence source that sets `(section, subsection, name)`, if any. Its own
	/// last-writer-wins lookup then yields the effective value, matching git's precedence.
	fn winning_source(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Option<&GitConfigSource> {
		self
			.sources
			.iter()
			.rev()
			.find(|source| source.contains(section, subsection, name))
	}

	/// The source that edits are directed at.
	fn writable_mut(&mut self) -> &mut GitConfigSource {
		&mut self.sources[self.writable]
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn source(text: &str) -> GitConfigSource {
		GitConfigSource::parse(text).unwrap()
	}

	/// A three-layer stack in read order: system, global, local (local highest precedence, writable).
	fn layered(system: &str, global: &str, local: &str) -> GitConfig {
		GitConfig::from_sources(vec![source(system), source(global), source(local)])
	}

	#[test]
	fn single_valued_lookup_takes_the_highest_precedence_source() {
		let config = layered(
			"[user]\n\tname = System\n\temail = sys@x\n",
			"[user]\n\tname = Global\n",
			"[user]\n\tname = Local\n",
		);
		// Local wins over global wins over system for a key each sets.
		assert_eq!(config.get_string("user", None, "name"), Some("Local"));
		// A key only a lower layer sets still resolves.
		assert_eq!(config.get_string("user", None, "email"), Some("sys@x"));
		// Unset everywhere is None.
		assert_eq!(config.get_string("user", None, "signingkey"), None);
	}

	#[test]
	fn multivars_concatenate_across_layers_in_read_order() {
		let config = layered(
			"[remote \"o\"]\n\tfetch = sys\n",
			"[remote \"o\"]\n\tfetch = glob\n",
			"[remote \"o\"]\n\tfetch = loc1\n\tfetch = loc2\n",
		);
		// System, then global, then both local values — the file read order.
		assert_eq!(
			config.get_all("remote", Some("o"), "fetch"),
			vec!["sys", "glob", "loc1", "loc2"]
		);
	}

	#[test]
	fn typed_getters_interpret_the_effective_value() {
		let config = layered(
			"[core]\n\tbare = true\n\tbig = 1\n",
			"[core]\n\tbare = false\n",
			"[core]\n\tbig = 2k\n",
		);
		// bare: global (false) overrides system (true); no local value.
		assert_eq!(config.get_bool("core", None, "bare").unwrap(), Some(false));
		// big: local (2k) overrides system; k-suffix interpreted.
		assert_eq!(config.get_int("core", None, "big").unwrap(), Some(2048));
		assert_eq!(config.get_bool("core", None, "missing").unwrap(), None);
	}

	#[test]
	fn get_bool_validated_checks_every_source_not_just_the_winner() {
		// A malformed *shadowed* (lower-precedence) value still aborts — git validates every occurrence of a
		// startup `core.*` bool — while the lazy `get_bool` accepts it (it only parses the winning source).
		let shadowed = layered(
			"[core]\n\tignorecase = bogus\n", // system: malformed
			"",
			"[core]\n\tignorecase = true\n", // local: valid, wins
		);
		assert_eq!(
			shadowed.get_bool("core", None, "ignorecase").unwrap(),
			Some(true)
		);
		assert!(
			shadowed
				.get_bool_validated("core", None, "ignorecase")
				.is_err()
		);

		// Every value valid: returns the winner (highest-precedence).
		let ok = layered(
			"[core]\n\tignorecase = false\n",
			"",
			"[core]\n\tignorecase = true\n",
		);
		assert_eq!(
			ok.get_bool_validated("core", None, "ignorecase").unwrap(),
			Some(true)
		);

		// Unset anywhere: None.
		assert_eq!(
			layered("", "", "")
				.get_bool_validated("core", None, "ignorecase")
				.unwrap(),
			None
		);
	}

	#[test]
	fn get_int_validated_checks_every_source_not_just_the_winner() {
		// A malformed *shadowed* (lower-precedence) integer still aborts — git validates every occurrence of a
		// startup `core.*` int (e.g. `repositoryformatversion`) — while the lazy `get_int` accepts it.
		let shadowed = layered(
			"[core]\n\trepositoryformatversion = notanint\n", // system: malformed
			"",
			"[core]\n\trepositoryformatversion = 1\n", // local: valid, wins
		);
		assert_eq!(
			shadowed
				.get_int("core", None, "repositoryformatversion")
				.unwrap(),
			Some(1)
		);
		assert!(
			shadowed
				.get_int_validated("core", None, "repositoryformatversion")
				.is_err()
		);

		// Every value valid: returns the winner (highest-precedence).
		let ok = layered(
			"[core]\n\trepositoryformatversion = 0\n",
			"",
			"[core]\n\trepositoryformatversion = 1\n",
		);
		assert_eq!(
			ok.get_int_validated("core", None, "repositoryformatversion")
				.unwrap(),
			Some(1)
		);

		// Unset anywhere: None.
		assert_eq!(
			layered("", "", "")
				.get_int_validated("core", None, "repositoryformatversion")
				.unwrap(),
			None
		);
	}

	#[test]
	fn a_higher_layer_can_mask_a_lower_value_with_a_bare_variable() {
		// git: an empty/bare override in a higher-precedence file shadows a lower value.
		let config = layered("", "[user]\n\tname = Global\n", "[user]\n\tname\n");
		// The local bare variable is the effective (highest-precedence) definition, so get_string
		// reports no value even though global sets one.
		assert_eq!(config.get_string("user", None, "name"), None);
		// get_raw distinguishes it from absent: present, but valueless.
		assert_eq!(config.get_raw("user", None, "name"), Some(None));
	}

	#[test]
	fn writes_and_render_target_the_writable_source_only() {
		let mut config = layered(
			"[user]\n\tname = System\n",
			"[user]\n\tname = Global\n",
			"[user]\n\tname = Local\n",
		);
		config.set("user", None, "name", "Edited").unwrap();
		// The effective value updates...
		assert_eq!(config.get_string("user", None, "name"), Some("Edited"));
		// ...and only the local file is rendered, carrying the edit; lower layers are untouched.
		assert_eq!(config.render(), "[user]\n\tname = Edited\n");
	}

	#[test]
	fn entries_list_every_layer_in_read_order() {
		let config = layered(
			"[core]\n\tbare = false\n",
			"[user]\n\temail = g@x\n",
			"[user]\n\tname = Local\n",
		);
		let entries: Vec<_> = config.entries();
		assert_eq!(
			entries,
			vec![
				("core.bare".to_owned(), Some("false")),
				("user.email".to_owned(), Some("g@x")),
				("user.name".to_owned(), Some("Local")),
			]
		);
	}

	#[test]
	fn subsections_merge_first_seen_across_layers() {
		let config = layered(
			"[remote \"origin\"]\n\turl = a\n",
			"[remote \"upstream\"]\n\turl = b\n",
			"[remote \"origin\"]\n\tfetch = c\n",
		);
		// `origin` appears first (system), `upstream` next (global); the local re-mention of `origin`
		// does not duplicate it.
		assert_eq!(config.subsections("remote"), vec!["origin", "upstream"]);
	}

	#[test]
	fn underlay_slips_lower_layers_under_the_writable_source() {
		// Start from a repo-local config (writable), then layer global + system beneath it.
		let mut config = GitConfig::parse("[user]\n\tname = Local\n").unwrap();
		config.underlay(vec![
			source("[user]\n\tname = System\n\temail = sys@x\n"),
			source("[user]\n\tname = Global\n"),
		]);
		// Local still wins; the underlaid email now resolves.
		assert_eq!(config.get_string("user", None, "name"), Some("Local"));
		assert_eq!(config.get_string("user", None, "email"), Some("sys@x"));
		// Writes still land in the (still-writable) local source, not a lower layer.
		config.set("user", None, "name", "Edited").unwrap();
		assert_eq!(config.render(), "[user]\n\tname = Edited\n");
	}

	#[test]
	fn single_source_config_behaves_like_the_underlying_source() {
		let mut config = GitConfig::new();
		config.set("user", None, "name", "A U Thor").unwrap();
		config.set("user", None, "email", "a@example.com").unwrap();
		assert_eq!(config.get_string("user", None, "name"), Some("A U Thor"));
		assert_eq!(
			GitConfigSource::parse(&config.render()).unwrap().render(),
			config.render()
		);
	}
}
