use std::future::Future;
use std::ops::Range;
use std::path::Path;
use std::pin::Pin;

use crate::{
	ConfigError, IncludeContext, IncludeResolver, condition_matches, is_hasconfig_remote_url, parser,
	resolve_include_path,
};

/// git's maximum include nesting depth; exceeding it is fatal (and breaks any cycle).
const MAX_INCLUDE_DEPTH: usize = 10;

/// The two directories git resolves a config file's includes against (see
/// [`GitConfigSource::expand_includes`]): the **lexical** parent (for a relative `include.path`) and the
/// **real** symlink-resolved parent (for a `gitdir:./` condition). They differ only when the file is
/// reached via a symlink; bundling them keeps the recursion helpers' signatures small.
///
/// `fileless` marks a **command-scope** source (`-c` / `GIT_CONFIG_*`), which has no containing file: a
/// relative `include.path` is then fatal and a `gitdir:./` condition does not match, as in git (see
/// [`GitConfigSource::expand_includes_command_scope`]). It is always `false` for an included file (which
/// necessarily has a path) and for an ordinary on-disk config layer.
#[derive(Clone, Copy)]
struct IncludeDirs<'a> {
	lexical: &'a Path,
	real: &'a Path,
	fileless: bool,
}

/// One structural piece of a config file, retaining its exact source text so the file round-trips
/// and edits stay surgical. Concatenating every element's raw text reproduces the source verbatim.
#[derive(Debug, Clone)]
pub(crate) enum Element {
	/// Blank lines, whole-line comments, and inter-line gaps — preserved verbatim.
	Filler(String),
	/// A `[section]` header line.
	Section(Section),
	/// A `name = value` (or bare `name`) variable line.
	Variable(Variable),
}

impl Element {
	/// The element's retained raw source text.
	fn raw_mut(&mut self) -> &mut String {
		match self {
			Element::Filler(raw) => raw,
			Element::Section(section) => &mut section.raw,
			Element::Variable(variable) => &mut variable.raw,
		}
	}

	/// The element's retained raw source text.
	fn raw(&self) -> &str {
		match self {
			Element::Filler(raw) => raw,
			Element::Section(section) => &section.raw,
			Element::Variable(variable) => &variable.raw,
		}
	}
}

/// Where an [`Element`] came from: parsed directly from this file, or spliced in from an included
/// file. Reads see both; writes and serialization touch only `Own` elements — git never flattens an
/// included file's content into the including file, and a write must land only in the target file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElementOrigin {
	/// Parsed directly from this file's own text.
	Own,
	/// Spliced in by include expansion; read-only, never written back or serialized.
	Included,
}

/// An [`Element`] tagged with its [`ElementOrigin`], as stored in a [`GitConfigSource`].
#[derive(Debug, Clone)]
pub(crate) struct Entry {
	element: Element,
	origin: ElementOrigin,
}

impl Entry {
	/// A directly-parsed (own) entry.
	fn own(element: Element) -> Self {
		Self {
			element,
			origin: ElementOrigin::Own,
		}
	}

	/// Whether this entry is `Own` (visible to writes and serialization).
	fn is_own(&self) -> bool {
		self.origin == ElementOrigin::Own
	}
}

/// A section header. `section` is lower-cased (git folds its case); `subsection` is verbatim
/// (case-sensitive). `raw` is the whole physical header line, including indentation, any trailing
/// comment, and the terminating newline.
#[derive(Debug, Clone)]
pub(crate) struct Section {
	pub section: String,
	pub subsection: Option<String>,
	pub raw: String,
}

/// One variable. `section` and `name` are stored lower-cased (git folds their case); `subsection`
/// is kept verbatim. `section`/`subsection` are denormalised from the enclosing header so lookups
/// need no back-reference. A `None` value is a boolean-true variable (`name` with no `= value`).
///
/// `raw` is the whole logical variable line (leading indentation through the terminating newline,
/// including any continuation lines). `value_span` is the byte range of the value's text within
/// `raw`, so a writer replaces just the value in place; it is `None` for a bare variable.
#[derive(Debug, Clone)]
pub(crate) struct Variable {
	pub section: String,
	pub subsection: Option<String>,
	pub name: String,
	pub value: Option<String>,
	pub raw: String,
	pub value_span: Option<Range<usize>>,
}

/// The result of git's remote-URL pre-scan ([`GitConfigSource::scan_remote_urls`]). A driver runs the
/// scan per layer and combines the results across the whole config, because git resolves `hasconfig`
/// (both its URL set and its paradox guard) against every precedence layer at once.
#[derive(Debug, Default, Clone)]
pub struct RemoteUrlScan {
	/// Every `remote.<name>.url` value reachable through the top level and plain `[include]`s — the
	/// regions a real read collects from. URLs reachable only through an `includeIf` are never here
	/// (they are the paradox instead). These feed `IncludeContext::remote_urls` for the real expansion.
	pub urls: Vec<String>,
	/// Whether any `hasconfig:remote.*.url:` directive was reached. This is git's trigger: the pre-scan
	/// (and hence the paradox guard) only takes effect when a `hasconfig` condition exists to evaluate.
	pub has_hasconfig: bool,
	/// Whether a *matched* `includeIf` subtree set a `remote.<name>.url` (git's `forbid_remote_url`).
	/// Recorded rather than fatalled here so the driver can combine it with `has_hasconfig` from any
	/// layer: the config is rejected exactly when both hold.
	pub forbidden_url: bool,
}

/// A single parsed git configuration file: an ordered list of elements with case-correct,
/// multi-value-aware lookups. Writes are surgical — they preserve comments and the surrounding
/// layout. [`include`](Self::expand_includes) / `includeIf` directives are expanded on request.
///
/// This models exactly one file (`.git/config`, `~/.gitconfig`, `/etc/gitconfig`, …). To resolve a
/// value across git's precedence stack of several such files, layer them in a [`GitConfig`](crate::GitConfig).
#[derive(Debug, Clone, Default)]
pub struct GitConfigSource {
	elements: Vec<Entry>,
}

/// Two configs are equal when they carry the same ordered variables (section, subsection, name,
/// value); comments, blank lines, and formatting do not affect equality.
impl PartialEq for GitConfigSource {
	fn eq(&self, other: &Self) -> bool {
		self.logical().eq(other.logical())
	}
}

impl Eq for GitConfigSource {}

impl GitConfigSource {
	/// An empty config.
	pub fn new() -> Self {
		Self::default()
	}

	/// Parse git config text.
	pub fn parse(text: &str) -> Result<Self, ConfigError> {
		parser::parse(text)
	}

	pub(crate) fn from_elements(elements: Vec<Element>) -> Self {
		Self {
			elements: elements.into_iter().map(Entry::own).collect(),
		}
	}

	/// Expand `[include]` / `[includeIf]` directives in place, splicing each included file's
	/// (already-expanded) contents *immediately after* the directive so ordering and last-value-wins
	/// are preserved — exactly as git does while parsing. The directive line stays queryable as an
	/// ordinary `include.path` / `includeif.*.path` key (git keeps it), and the spliced-in content is
	/// tagged [`ElementOrigin::Included`] so it is visible to reads but ignored by writes and
	/// serialization (a write must not flatten another file's content into this one).
	///
	/// `dir` is the including file's **lexical** directory (the path it was reached through), used to
	/// resolve a relative `include.path`; `real_dir` is its **real** (symlink-resolved) directory, used
	/// by a `gitdir:./` condition. git resolves the two differently — a relative include against the
	/// file's lexical dir, a `./` condition against its realpath — so a config reached via a symlink
	/// resolves each the way git does. `ctx` supplies `$HOME`/gitdir/branch; `resolver` performs the
	/// reads (returning `None` for an absent file, which is silently skipped) and reports each read
	/// file's canonical path so nested includes get the same split treatment.
	///
	/// Errors: a bare `path` on a directive that would be read ([`ConfigError::IncludeMissingValue`]);
	/// a `~/` path with no `$HOME`, or an unsupported `~user/`, on a matched directive
	/// ([`ConfigError::IncludeTildeNoHome`] / [`ConfigError::IncludeUserTildeUnsupported`]); or nesting
	/// past git's depth of 10 ([`ConfigError::IncludeDepthExceeded`], which also breaks a cycle).
	///
	/// # Expand-vs-mutate contract
	///
	/// Mutating an `include`/`includeif` directive on an already-expanded source (`set`/`unset`/…) does
	/// **not** re-run the include, so the previously spliced [`ElementOrigin::Included`] entries linger
	/// as stale reads until the next expansion. The intended consumer design avoids this: the *writable*
	/// source stays raw (unexpanded) and reads use separate expanded copies — so a caller must not both
	/// expand and mutate include directives on the same source. This method is idempotent, so
	/// re-calling it after an include change is the supported way to refresh.
	pub async fn expand_includes<R: IncludeResolver>(
		&mut self,
		dir: &Path,
		real_dir: &Path,
		ctx: &IncludeContext<'_>,
		resolver: &R,
	) -> Result<(), ConfigError> {
		// Idempotent: drop any content spliced in by a previous expansion before re-expanding, so a
		// second call neither duplicates an unchanged include nor lets a stale value outlive a changed
		// one. Only own elements survive to be (re-)expanded.
		self.elements.retain(Entry::is_own);
		let dirs = IncludeDirs {
			lexical: dir,
			real: real_dir,
			fileless: false,
		};
		self.expand_includes_at_depth(dirs, ctx, resolver, 0).await
	}

	/// Expand includes in a **command-scope** source — `-c` / `GIT_CONFIG_*` config, which git reads as
	/// part of the same include-aware sequence but which has no containing file. git still expands an
	/// absolute (or `~`-based) `include.path` here, so this does too; but a *relative* `include.path` is
	/// fatal ([`ConfigError::IncludeRelativeFromCommandScope`]) and a `gitdir:./` condition simply does
	/// not match — both matching git, which has no file to resolve either against. Otherwise identical to
	/// [`expand_includes`](Self::expand_includes) (idempotent, same depth cap, same origin tagging).
	pub async fn expand_includes_command_scope<R: IncludeResolver>(
		&mut self,
		ctx: &IncludeContext<'_>,
		resolver: &R,
	) -> Result<(), ConfigError> {
		self.elements.retain(Entry::is_own);
		let empty = Path::new("");
		let dirs = IncludeDirs {
			lexical: empty,
			real: empty,
			fileless: true,
		};
		self.expand_includes_at_depth(dirs, ctx, resolver, 0).await
	}

	/// The depth-tracking core of [`expand_includes`](Self::expand_includes). Returns a boxed `Send`
	/// future so the async body can recurse (an `async fn` cannot name its own future to recurse) and
	/// so the whole chain stays `Send`, matching the crate's Send-futures convention.
	///
	/// This pass no longer enforces git's `hasconfig` URL-paradox guard: that is the pre-scan's job
	/// ([`scan_remote_urls`](Self::scan_remote_urls)), which git runs *before* the real read whenever a
	/// `hasconfig` condition is reached, and which alone sees every layer (the guard is whole-config).
	/// A driver therefore runs `scan_remote_urls` first — it supplies `ctx.remote_urls` *and* fatals on
	/// the paradox — so by the time this expansion runs, a paradoxical config has already been rejected.
	fn expand_includes_at_depth<'a, R: IncludeResolver>(
		&'a mut self,
		dirs: IncludeDirs<'a>,
		ctx: &'a IncludeContext<'a>,
		resolver: &'a R,
		depth: usize,
	) -> Pin<Box<dyn Future<Output = Result<(), ConfigError>> + Send + 'a>> {
		Box::pin(async move {
			let mut i = 0;
			while i < self.elements.len() {
				// Only own directives are expandable: content spliced in from a nested include was
				// already expanded in its own pass, and must never be re-read here.
				if !self.elements[i].is_own() {
					i += 1;
					continue;
				}
				let Some((value, condition)) = include_directive(&self.elements[i].element) else {
					i += 1;
					continue;
				};

				// The directive itself stays in place (git keeps `include.path` / `includeif.*.path`
				// queryable as an ordinary key while ALSO expanding), so every skip path just advances
				// past it; only a matched directive splices the included content immediately after it.

				// git reads the path only when an `includeIf` condition matches. `gitdir:./` in the
				// condition resolves against the file's real directory (or never matches in command scope).
				if let Some(condition) = &condition
					&& !condition_matches(condition, dirs.real, ctx, dirs.fileless)
				{
					i += 1;
					continue;
				}
				// The directive will be read: a bare `path` with no value is fatal, as in git.
				let Some(value) = value else {
					return Err(ConfigError::IncludeMissingValue);
				};
				// A matched directive whose path needs `~/` but has no `$HOME`, or an unsupported
				// `~user/`, is fatal in git — distinct from an absent file, which is skipped. A relative
				// path resolves against the *lexical* directory, as git does (or is fatal in command scope).
				let resolved = resolve_include_path(&value, dirs.lexical, ctx.home, dirs.fileless)?;
				// git's access check precedes its depth check: an absent target is silently skipped
				// (no depth error), and only a target that actually READS triggers the depth cap.
				let Some((text, canonical)) = resolver.read(&resolved).await? else {
					i += 1;
					continue;
				};
				if depth + 1 > MAX_INCLUDE_DEPTH {
					return Err(ConfigError::IncludeDepthExceeded);
				}

				// Expand the included file before splicing, so nested includes land inline at the right
				// positions. Its relative includes resolve against its own *lexical* dir (the path we
				// reached it through), its `./` conditions against its *real* dir (from the canonical path
				// the resolver reported) — git's split treatment, one level down.
				let mut included = GitConfigSource::parse(&text)?;
				let included_dir = resolved.parent().map(Path::to_path_buf).unwrap_or_default();
				let included_real_dir = canonical
					.parent()
					.map(Path::to_path_buf)
					.unwrap_or_default();
				// The included file has a path, so its own includes are never fileless (even when the
				// including source was command-scope: only an absolute path could have been read here).
				let included_dirs = IncludeDirs {
					lexical: &included_dir,
					real: &included_real_dir,
					fileless: false,
				};
				included
					.expand_includes_at_depth(included_dirs, ctx, resolver, depth + 1)
					.await?;

				// Everything from the included file is non-own from this file's perspective, so reads
				// see it but writes/serialization ignore it (git keeps the directive on disk, never the
				// flattened content). Because included elements are never rendered, no line-boundary
				// fix-up is needed at the seam — `render` emits only own elements, which stay contiguous.
				let mut spliced = included.elements;
				for entry in &mut spliced {
					entry.origin = ElementOrigin::Included;
				}
				let count = spliced.len();
				// Splice the included content AFTER the directive (at `i + 1`), leaving the directive at
				// `i` in place and queryable.
				self.elements.splice(i + 1..i + 1, spliced);
				i += 1 + count;
			}
			Ok(())
		})
	}

	/// git's remote-URL pre-scan (`populate_remote_urls`), the companion to
	/// [`expand_includes`](Self::expand_includes). git runs it — over the *whole* config — the first
	/// time a `hasconfig:remote.*.url:` condition is reached during a read, both to learn which remote
	/// URLs exist (so the condition can be evaluated) and to enforce the paradox guard. It reads all
	/// config with `hasconfig` conditions **forced true** (`unconditional_remote_url`), so:
	///
	/// - **URLs** ([`RemoteUrlScan::urls`]) are collected from the top level and any plain `[include]`
	///   subtree — the regions a real read would collect from — but never from an `includeIf` subtree.
	/// - Entering *any matched* `includeIf` subtree (a real-matching `gitdir:`/`onbranch:`, or a
	///   forced-true `hasconfig:`) arms git's `forbid_remote_url`: a `remote.<name>.url` anywhere inside
	///   is the paradox. This is recorded in [`RemoteUrlScan::forbidden_url`] rather than fatalled
	///   immediately, because the *trigger* ([`RemoteUrlScan::has_hasconfig`], set when any `hasconfig`
	///   directive is reached) may live in a **different layer**; only the driver, seeing every layer,
	///   knows whether a `hasconfig` exists at all, and so combines the per-layer scans and fatals with
	///   [`ConfigError::HasconfigIncludeSetsRemoteUrl`] exactly when `has_hasconfig && forbidden_url`.
	///
	/// Because `hasconfig` is forced true here, this scan never consults `ctx.remote_urls` (it uses only
	/// `home`/`gitdir`/`branch`). It does not mutate `self`; it parses included files on the fly.
	///
	/// Deferred divergence: a valueless (`path` with no value) include reached *only* via the forced-true
	/// arm — i.e. inside a `hasconfig` subtree that would not really match — is skipped here rather than
	/// fatalling `missing value`, as git's forced pre-scan would. The real read reports it on a genuinely
	/// matched path; the gap is a `hasconfig`-subtree-only, already-broken-config edge (HLD Deferred).
	pub async fn scan_remote_urls<R: IncludeResolver>(
		&self,
		dir: &Path,
		real_dir: &Path,
		ctx: &IncludeContext<'_>,
		resolver: &R,
	) -> Result<RemoteUrlScan, ConfigError> {
		let mut scan = RemoteUrlScan::default();
		let dirs = IncludeDirs {
			lexical: dir,
			real: real_dir,
			fileless: false,
		};
		self
			.scan_remote_urls_at_depth(dirs, ctx, resolver, 0, false, &mut scan)
			.await?;
		Ok(scan)
	}

	/// [`scan_remote_urls`](Self::scan_remote_urls) for a **command-scope** source (`-c` / `GIT_CONFIG_*`).
	/// git includes command-scope `remote.<name>.url` in the same pre-scan, so a URL set only via `-c` can
	/// satisfy a file-level `hasconfig` condition (and is subject to the same paradox guard). The
	/// command-scope source has no file, so — as in [`expand_includes_command_scope`](Self::expand_includes_command_scope)
	/// — a relative `include.path` is fatal and a `gitdir:./` condition does not match.
	pub async fn scan_remote_urls_command_scope<R: IncludeResolver>(
		&self,
		ctx: &IncludeContext<'_>,
		resolver: &R,
	) -> Result<RemoteUrlScan, ConfigError> {
		let mut scan = RemoteUrlScan::default();
		let empty = Path::new("");
		let dirs = IncludeDirs {
			lexical: empty,
			real: empty,
			fileless: true,
		};
		self
			.scan_remote_urls_at_depth(dirs, ctx, resolver, 0, false, &mut scan)
			.await?;
		Ok(scan)
	}

	/// The depth-tracking core of [`scan_remote_urls`](Self::scan_remote_urls). Boxed-`Send` for the same
	/// reason as [`expand_includes_at_depth`](Self::expand_includes_at_depth) — an `async fn` cannot name
	/// its own future to recurse. `forbidden` is set once inside a matched `includeIf` subtree (and
	/// propagates through nested plain includes), so a `remote.<name>.url` there is recorded as the
	/// paradox rather than collected.
	fn scan_remote_urls_at_depth<'a, R: IncludeResolver>(
		&'a self,
		dirs: IncludeDirs<'a>,
		ctx: &'a IncludeContext<'a>,
		resolver: &'a R,
		depth: usize,
		forbidden: bool,
		scan: &'a mut RemoteUrlScan,
	) -> Pin<Box<dyn Future<Output = Result<(), ConfigError>> + Send + 'a>> {
		Box::pin(async move {
			for entry in &self.elements {
				let element = &entry.element;
				// A `remote.<name>.url` is the paradox inside a matched-includeIf subtree, else a
				// collectable URL. git forbids on the *key* (so a valueless one still trips it) but only
				// a value can be matched against a `hasconfig` glob, so only a value is collected.
				if element_is_remote_subsection_url(element) {
					if forbidden {
						scan.forbidden_url = true;
					} else if let Element::Variable(v) = element
						&& let Some(value) = &v.value
					{
						scan.urls.push(value.clone());
					}
					continue;
				}
				// Reaching any `hasconfig:remote.*.url:` directive (regardless of its key, per git's
				// evaluate-before-key-check order) is what triggers the whole pre-scan.
				if element_is_hasconfig_includeif(element) {
					scan.has_hasconfig = true;
				}
				let Some((value, condition)) = include_directive(element) else {
					continue;
				};
				// Decide whether to descend, and whether the subtree forbids remote URLs. A plain
				// `[include]` inherits the parent's state; a `hasconfig:` is forced true (and arms the
				// forbid); a `gitdir:`/`onbranch:` is evaluated by its real condition and, if it matches,
				// arms the forbid.
				let child_forbidden = match &condition {
					None => forbidden,
					Some(cond) if is_hasconfig_remote_url(cond) => true,
					Some(cond) => {
						// `gitdir:./` resolves against the file's real directory (or never matches in command
						// scope), as git does.
						if !condition_matches(cond, dirs.real, ctx, dirs.fileless) {
							continue;
						}
						true
					}
				};
				// A matched directive with no path value can't be read (see the deferred divergence on
				// `scan_remote_urls`); skip it here — the real expand pass reports `missing value` on a
				// genuinely matched path.
				let Some(value) = value else {
					continue;
				};
				// A relative `include.path` resolves against the *lexical* dir; nested `./` conditions use
				// the *real* dir taken from the resolver's canonical path — git's split treatment. A relative
				// path in command scope is fatal (as in `expand_includes`).
				let resolved = resolve_include_path(&value, dirs.lexical, ctx.home, dirs.fileless)?;
				// Access check precedes the depth check, matching `expand_includes` (and git).
				let Some((text, canonical)) = resolver.read(&resolved).await? else {
					continue;
				};
				if depth + 1 > MAX_INCLUDE_DEPTH {
					return Err(ConfigError::IncludeDepthExceeded);
				}
				let included = GitConfigSource::parse(&text)?;
				let included_dir = resolved.parent().map(Path::to_path_buf).unwrap_or_default();
				let included_real_dir = canonical
					.parent()
					.map(Path::to_path_buf)
					.unwrap_or_default();
				// A read included file has a path, so its nested includes are never fileless.
				let included_dirs = IncludeDirs {
					lexical: &included_dir,
					real: &included_real_dir,
					fileless: false,
				};
				included
					.scan_remote_urls_at_depth(
						included_dirs,
						ctx,
						resolver,
						depth + 1,
						child_forbidden,
						scan,
					)
					.await?;
			}
			Ok(())
		})
	}

	/// Every variable element, in file order. Reads see all elements — own **and** included — so a
	/// value set only through an `[include]` resolves like git.
	fn variables(&self) -> impl Iterator<Item = &Variable> {
		self.elements.iter().filter_map(|e| match &e.element {
			Element::Variable(v) => Some(v),
			_ => None,
		})
	}

	/// Every *own* variable element with its index, in file order — the write-side view (included
	/// elements are read-only and never edited or removed). All mutators (`set`/`unset`/`replace_all`/
	/// `add`/`remove_subsection`/`rename_subsection`) funnel through this own-only view; per the
	/// expand-vs-mutate contract on [`expand_includes`](Self::expand_includes), mutating an include
	/// directive on an expanded source leaves its spliced content stale until a re-expand.
	fn own_variables(&self) -> impl Iterator<Item = (usize, &Variable)> {
		self
			.elements
			.iter()
			.enumerate()
			.filter_map(|(i, e)| match &e.element {
				Element::Variable(v) if e.is_own() => Some((i, v)),
				_ => None,
			})
	}

	/// Logical projection of each variable, for equality.
	fn logical(&self) -> impl Iterator<Item = (&str, Option<&str>, &str, Option<&str>)> {
		self.variables().map(|v| {
			(
				v.section.as_str(),
				v.subsection.as_deref(),
				v.name.as_str(),
				v.value.as_deref(),
			)
		})
	}

	fn matches<'a>(
		&'a self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> impl Iterator<Item = &'a Variable> {
		let section = section.to_ascii_lowercase();
		let name = name.to_ascii_lowercase();
		let subsection = subsection.map(str::to_owned);
		self.variables().filter(move |v| {
			v.section == section && v.name == name && v.subsection.as_deref() == subsection.as_deref()
		})
	}

	/// Whether this file sets `(section, subsection, name)` at all (with any value, including a bare
	/// boolean-true). Lets a layered [`GitConfig`](crate::GitConfig) find the highest-precedence file
	/// that defines a key before delegating the typed interpretation back to it.
	pub(crate) fn contains(&self, section: &str, subsection: Option<&str>, name: &str) -> bool {
		self.matches(section, subsection, name).next().is_some()
	}

	/// The last value for a variable, or `None` if unset (or set as boolean-true).
	pub fn get_string(&self, section: &str, subsection: Option<&str>, name: &str) -> Option<&str> {
		self
			.matches(section, subsection, name)
			.last()
			.and_then(|v| v.value.as_deref())
	}

	/// All values for a multi-valued variable, in order.
	pub fn get_all(&self, section: &str, subsection: Option<&str>, name: &str) -> Vec<&str> {
		self
			.matches(section, subsection, name)
			.filter_map(|v| v.value.as_deref())
			.collect()
	}

	/// The last matching variable's value, keeping "absent" distinct from "present but valueless":
	/// outer `None` if the variable is unset, inner `None` for a bare (valueless) variable.
	pub fn get_raw(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Option<Option<&str>> {
		self
			.matches(section, subsection, name)
			.last()
			.map(|v| v.value.as_deref())
	}

	/// Every matching variable in order (inner `None` for a valueless one); empty if unset.
	pub fn get_all_raw(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Vec<Option<&str>> {
		self
			.matches(section, subsection, name)
			.map(|v| v.value.as_deref())
			.collect()
	}

	/// Interpret the last value as a git boolean: `true/yes/on` (or a bare name) → true; `false/no/off/""` →
	/// false; otherwise git's numeric grammar — any base-0 integer (`0x` hex, leading-`0` octal, else decimal,
	/// with an optional `k`/`m`/`g` multiplier and sign), non-zero → true, zero → false. `None` if unset; an
	/// error for a non-boolean, non-numeric value.
	pub fn get_bool(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Result<Option<bool>, ConfigError> {
		match self.matches(section, subsection, name).last() {
			None => Ok(None),
			Some(v) => interpret_bool(v.value.as_deref()).map(Some),
		}
	}

	/// Interpret the last value as a git integer (optional `k`/`m`/`g` 1024-multiplier). A bare
	/// (valueless) variable is present, not absent, so it interprets as `""` — a parse error,
	/// as git reports — rather than `None`.
	pub fn get_int(
		&self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
	) -> Result<Option<i64>, ConfigError> {
		match self.get_raw(section, subsection, name) {
			None => Ok(None),
			Some(value) => interpret_int(value.unwrap_or("")).map(Some),
		}
	}

	/// Set a variable to a single value, editing in place where possible.
	///
	/// If the variable exists exactly once, its value text is spliced in place (preserving the key,
	/// indentation, and any trailing comment). If it does not exist, a new line is inserted into the
	/// (last) matching section, creating the section at end of file if needed. If it already holds
	/// **multiple** values, this refuses with [`ConfigError::MultipleValues`] and leaves the config
	/// unchanged — matching git, which will not collapse a multi-valued variable on a plain set.
	pub fn set(
		&mut self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
		value: &str,
	) -> Result<(), ConfigError> {
		let section = section.to_ascii_lowercase();
		let name = name.to_ascii_lowercase();
		let subsection = subsection.map(str::to_owned);

		let matches: Vec<usize> = self
			.matching_indices(&section, subsection.as_deref(), &name)
			.collect();

		if matches.len() > 1 {
			return Err(ConfigError::MultipleValues(dotted_key(
				&section,
				subsection.as_deref(),
				&name,
			)));
		}

		if let Some(&idx) = matches.first() {
			self.replace_value_in_place(idx, &section, subsection.as_deref(), &name, value);
			return Ok(());
		}

		let var = synth_variable(&section, subsection.as_deref(), &name, value);
		self.insert_variable(&section, subsection.as_deref(), var);
		Ok(())
	}

	/// Replace every value of a variable with a single value (`config --replace-all`).
	///
	/// Unlike [`set`](Self::set), this is willing to collapse a multi-valued variable: the first
	/// occurrence is edited in place (preserving its key, indentation, and trailing comment) and any
	/// further occurrences are removed. If the variable does not exist, the value is inserted as for
	/// `set`. (Value-pattern matching, git's optional `value_regex`, is not supported.)
	pub fn replace_all(&mut self, section: &str, subsection: Option<&str>, name: &str, value: &str) {
		let section = section.to_ascii_lowercase();
		let name = name.to_ascii_lowercase();
		let subsection = subsection.map(str::to_owned);

		let matches: Vec<usize> = self
			.matching_indices(&section, subsection.as_deref(), &name)
			.collect();

		if let Some((&first, rest)) = matches.split_first() {
			self.replace_value_in_place(first, &section, subsection.as_deref(), &name, value);
			// Drop the remaining occurrences (highest index first) so a single value remains.
			for &i in rest.iter().rev() {
				self.elements.remove(i);
			}
			return;
		}

		let var = synth_variable(&section, subsection.as_deref(), &name, value);
		self.insert_variable(&section, subsection.as_deref(), var);
	}

	/// Overwrite the value of the variable element at `idx` in place, splicing into its value span
	/// (preserving key/indentation/trailing comment) or, for a previously bare variable that has no
	/// value text, regenerating the line.
	fn replace_value_in_place(
		&mut self,
		idx: usize,
		section: &str,
		subsection: Option<&str>,
		name: &str,
		value: &str,
	) {
		if let Element::Variable(v) = &mut self.elements[idx].element {
			match v.value_span.clone() {
				Some(span) => {
					let quoted = quote_value(value);
					v.raw.replace_range(span.clone(), &quoted);
					v.value_span = Some(span.start..span.start + quoted.len());
					v.value = Some(value.to_owned());
				}
				None => {
					*v = synth_variable(section, subsection, name, value);
				}
			}
		}
	}

	/// Remove every value of a variable, returning whether anything was removed. The section header
	/// is left in place, as git does.
	pub fn unset(&mut self, section: &str, subsection: Option<&str>, name: &str) -> bool {
		let section = section.to_ascii_lowercase();
		let name = name.to_ascii_lowercase();
		let before = self.elements.len();
		// Remove only own matches; an included value stays (a write never edits another file).
		self.elements.retain(|e| match &e.element {
			Element::Variable(v) => {
				!(e.is_own()
					&& v.section == section
					&& v.name == name
					&& v.subsection.as_deref() == subsection)
			}
			_ => true,
		});
		self.elements.len() != before
	}

	/// Remove an entire subsection: its `[section "subsection"]` header line(s) and every variable in
	/// it. Returns whether anything was removed. Other sections and the surrounding layout are left
	/// in place. (Used to delete a whole `[remote "name"]`, which `unset` cannot do variable-blind.)
	pub fn remove_subsection(&mut self, section: &str, subsection: &str) -> bool {
		let section = section.to_ascii_lowercase();
		let before = self.elements.len();
		// Own elements only: an included subsection is read-only and stays put.
		self.elements.retain(|e| match &e.element {
			Element::Section(s) => {
				!(e.is_own() && s.section == section && s.subsection.as_deref() == Some(subsection))
			}
			Element::Variable(v) => {
				!(e.is_own() && v.section == section && v.subsection.as_deref() == Some(subsection))
			}
			Element::Filler(_) => true,
		});
		self.elements.len() != before
	}

	/// Rename a subsection: rewrite its `[section "old"]` header(s) to `[section "new"]` and re-tag
	/// every variable in it. Returns whether anything was renamed. The variable lines themselves are
	/// untouched (they do not carry the subsection name); other sections and layout are preserved.
	/// (Used to rename a `[remote "old"]` to `[remote "new"]`.)
	pub fn rename_subsection(&mut self, section: &str, old: &str, new: &str) -> bool {
		let section = section.to_ascii_lowercase();
		let mut renamed = false;
		// Rename own elements only; included ones are read-only.
		for entry in &mut self.elements {
			if !entry.is_own() {
				continue;
			}
			match &mut entry.element {
				Element::Section(s) if s.section == section && s.subsection.as_deref() == Some(old) => {
					*s = synth_section(&section, Some(new));
					renamed = true;
				}
				Element::Variable(v) if v.section == section && v.subsection.as_deref() == Some(old) => {
					v.subsection = Some(new.to_owned());
					renamed = true;
				}
				_ => {}
			}
		}
		renamed
	}

	/// The distinct subsection names that have *at least one variable* under `section`, in first-seen
	/// order (e.g. the configured remote names from `remote.<name>.*`). A bare `[section "sub"]`
	/// header with no variables is ignored, matching git — which treats an empty remote section as no
	/// remote at all.
	pub fn subsections(&self, section: &str) -> Vec<&str> {
		let section = section.to_ascii_lowercase();
		let mut names: Vec<&str> = Vec::new();
		for variable in self.variables() {
			if variable.section == section
				&& let Some(name) = variable.subsection.as_deref()
				&& !names.contains(&name)
			{
				names.push(name);
			}
		}
		names
	}

	/// Every variable named `name` under `section`, across all subsections, in file order — as
	/// `(subsection, value)` pairs (`subsection` is `None` for the section itself, `value` is `None`
	/// for a bare variable). Unlike [`Self::subsections`] + [`Self::get_all`], this preserves the
	/// order variables appear across interleaved subsection headers, which git relies on (e.g. to
	/// pick the first of several tied `url.*.insteadOf` rewrite rules).
	pub fn variables_named<'a>(
		&'a self,
		section: &str,
		name: &str,
	) -> Vec<(Option<&'a str>, Option<&'a str>)> {
		let section = section.to_ascii_lowercase();
		let name = name.to_ascii_lowercase();
		self
			.variables()
			.filter(|v| v.section == section && v.name == name)
			.map(|v| (v.subsection.as_deref(), v.value.as_deref()))
			.collect()
	}

	/// Append a variable as its **own** section block at the very end, preserving strict insertion
	/// order — unlike [`add`](Self::add), which groups a repeat into the last matching section. This is
	/// how command-scope config (`-c` / `GIT_CONFIG_*`) must be built: git reads those entries as a
	/// linear stream, so an `include.path` interleaved with repeats of a key
	/// (`-c x.v=a -c include.path=… -c x.v=b`) must keep that order — a grouped representation would move
	/// the include past a later same-section key and let it win when git's last entry (`b`) should. Each
	/// entry gets a fresh `[section]` header; the rendered text is more verbose but is never written to
	/// disk (command-scope config has no file), and `get`/`get_all` order is exactly the insertion order.
	pub fn append(
		&mut self,
		section: &str,
		subsection: Option<&str>,
		name: &str,
		value: Option<&str>,
	) {
		let section_lc = section.to_ascii_lowercase();
		let name_lc = name.to_ascii_lowercase();
		let var = match value {
			Some(value) => synth_variable(&section_lc, subsection, &name_lc, value),
			None => synth_bare_variable(&section_lc, subsection, &name_lc),
		};
		self.ensure_trailing_newline(self.elements.len());
		self
			.elements
			.push(Entry::own(Element::Section(synth_section(
				&section_lc,
				subsection,
			))));
		self.elements.push(Entry::own(Element::Variable(var)));
	}

	/// Append a value (for multi-valued variables); a `None` value is boolean-true. Never replaces
	/// an existing value: the new line is inserted into the (last) matching section, creating the
	/// section at end of file if it does not exist.
	pub fn add(&mut self, section: &str, subsection: Option<&str>, name: &str, value: Option<&str>) {
		let section = section.to_ascii_lowercase();
		let name = name.to_ascii_lowercase();
		let var = match value {
			Some(value) => synth_variable(&section, subsection, &name, value),
			None => synth_bare_variable(&section, subsection, &name),
		};
		self.insert_variable(&section, subsection, var);
	}

	/// All variables in order, as a dotted key (`section[.subsection].name`, with the section and
	/// name lower-cased) and its value (`None` for a boolean-true variable). For `--list`.
	pub fn entries(&self) -> impl Iterator<Item = (String, Option<&str>)> {
		self.variables().map(|v| {
			let key = match &v.subsection {
				Some(sub) => format!("{}.{}.{}", v.section, sub, v.name),
				None => format!("{}.{}", v.section, v.name),
			};
			(key, v.value.as_deref())
		})
	}

	/// Serialise back to git config text by concatenating each **own** element's retained raw text
	/// (included content is never written back — git keeps the `[include]` directive on disk, not the
	/// flattened target). For an unmodified, un-expanded config this reproduces the source byte-for-byte.
	pub fn render(&self) -> String {
		let mut out = String::new();
		for entry in &self.elements {
			if entry.is_own() {
				out.push_str(entry.element.raw());
			}
		}
		out
	}

	/// Indices of **own** variable elements matching `(section, subsection, name)`, in file order.
	/// Writes edit own elements only, so an included occurrence is never overwritten in place.
	fn matching_indices<'a>(
		&'a self,
		section: &'a str,
		subsection: Option<&'a str>,
		name: &'a str,
	) -> impl Iterator<Item = usize> + 'a {
		self.own_variables().filter_map(move |(i, v)| {
			(v.section == section && v.name == name && v.subsection.as_deref() == subsection).then_some(i)
		})
	}

	/// Insert a variable element into the last matching **own** section block (after its final
	/// variable, or directly after the header if it has none), synthesising the section at end of
	/// file when it does not yet exist. Included sections are ignored — a write never targets them.
	fn insert_variable(&mut self, section: &str, subsection: Option<&str>, var: Variable) {
		let header = self.elements.iter().rposition(|e| {
			e.is_own()
				&& matches!(&e.element, Element::Section(s)
					if s.section == section && s.subsection.as_deref() == subsection)
		});
		match header {
			Some(header) => {
				// Walk the block to its last own variable, stopping at the next own section header.
				let mut insert_at = header + 1;
				for (offset, e) in self.elements[header + 1..].iter().enumerate() {
					match &e.element {
						Element::Section(_) if e.is_own() => break,
						Element::Variable(_) if e.is_own() => insert_at = header + 1 + offset + 1,
						_ => {}
					}
				}
				// Without this, a final line lacking a newline would glue onto the inserted one.
				self.ensure_trailing_newline(insert_at);
				self
					.elements
					.insert(insert_at, Entry::own(Element::Variable(var)));
			}
			None => {
				// Likewise, separate a synthesised section from a file with no trailing newline.
				self.ensure_trailing_newline(self.elements.len());
				self
					.elements
					.push(Entry::own(Element::Section(synth_section(
						section, subsection,
					))));
				self.elements.push(Entry::own(Element::Variable(var)));
			}
		}
	}

	/// Ensure the last **own** element before index `at` ends with a newline, so a line inserted at
	/// `at` starts on its own line. Walks back past any `Included` entries — `render` drops those, so a
	/// newline added to one would vanish and glue the new line onto the last rendered (own) line. No-op
	/// when there is no own element before `at`.
	fn ensure_trailing_newline(&mut self, at: usize) {
		if let Some(idx) = self.elements[..at].iter().rposition(Entry::is_own) {
			let raw = self.elements[idx].element.raw_mut();
			if !raw.is_empty() && !raw.ends_with('\n') {
				raw.push('\n');
			}
		}
	}
}

/// A freshly rendered `name = value` variable line, with its value span recorded so a later `set`
/// can splice in place.
fn synth_variable(section: &str, subsection: Option<&str>, name: &str, value: &str) -> Variable {
	let quoted = quote_value(value);
	let prefix = format!("\t{name} = ");
	let value_span = prefix.len()..prefix.len() + quoted.len();
	let raw = format!("{prefix}{quoted}\n");
	Variable {
		section: section.to_owned(),
		subsection: subsection.map(str::to_owned),
		name: name.to_owned(),
		value: Some(value.to_owned()),
		raw,
		value_span: Some(value_span),
	}
}

/// A freshly rendered bare (boolean-true) `name` variable line.
fn synth_bare_variable(section: &str, subsection: Option<&str>, name: &str) -> Variable {
	Variable {
		section: section.to_owned(),
		subsection: subsection.map(str::to_owned),
		name: name.to_owned(),
		value: None,
		raw: format!("\t{name}\n"),
		value_span: None,
	}
}

/// Recognise an include directive: `[include] path` (unconditional, condition `None`) or
/// `[includeIf "<cond>"] path` (conditional, condition `Some(cond)`). Returns the directive's path
/// value — `None` for a bare `path` with no value, so the caller can reproduce git's fatal
/// "missing value" only when the directive would actually be read. Returns `None` for any other
/// element. Section names are already lower-cased by the parser, so the match is case-correct.
///
/// An unconditional `[include]` must carry **no** subsection: `[include "profile"] path` is the
/// ordinary key `include.profile.path`, which git does not read (only bare `[include]` is special).
fn include_directive(element: &Element) -> Option<(Option<String>, Option<String>)> {
	let Element::Variable(v) = element else {
		return None;
	};
	if v.name != "path" {
		return None;
	}
	match (v.section.as_str(), v.subsection.as_deref()) {
		("include", None) => Some((v.value.clone(), None)),
		("includeif", Some(condition)) => Some((v.value.clone(), Some(condition.to_owned()))),
		_ => None,
	}
}

/// Whether `element` is a `remote.<subsection>.url` variable — the shape git's `hasconfig` collects
/// and, inside a hasconfig-included file, forbids. A subsection is required: a bare `remote.url` does
/// not count, matching git's `remote.<name>.url` key parse. Section/name are already lower-cased.
fn element_is_remote_subsection_url(element: &Element) -> bool {
	matches!(
		element,
		Element::Variable(v) if v.section == "remote" && v.subsection.is_some() && v.name == "url"
	)
}

/// Whether `element` is an `includeIf "hasconfig:remote.*.url:…"` variable — of any key, not only
/// `path`. git evaluates the condition (triggering the remote-URL pre-scan) before it checks the key
/// is `path`, so even a stray `[includeIf "hasconfig:…"] other = x` triggers it. Section is lower-cased.
fn element_is_hasconfig_includeif(element: &Element) -> bool {
	matches!(
		element,
		Element::Variable(v)
			if v.section == "includeif"
				&& v.subsection.as_deref().is_some_and(is_hasconfig_remote_url)
	)
}

/// Format a `section[.subsection].name` dotted key for diagnostics.
fn dotted_key(section: &str, subsection: Option<&str>, name: &str) -> String {
	match subsection {
		Some(sub) => format!("{section}.{sub}.{name}"),
		None => format!("{section}.{name}"),
	}
}

/// A freshly rendered section header line.
fn synth_section(section: &str, subsection: Option<&str>) -> Section {
	let raw = match subsection {
		Some(sub) => format!("[{section} \"{}\"]\n", escape_subsection(sub)),
		None => format!("[{section}]\n"),
	};
	Section {
		section: section.to_owned(),
		subsection: subsection.map(str::to_owned),
		raw,
	}
}

pub(crate) fn interpret_bool(value: Option<&str>) -> Result<bool, ConfigError> {
	match value {
		None => Ok(true),
		Some(v) => match v.to_ascii_lowercase().as_str() {
			"true" | "yes" | "on" => Ok(true),
			"false" | "no" | "off" | "" => Ok(false),
			// git's numeric booleans: any integer (with an optional `k`/`m`/`g` 1024-multiplier and sign) is a
			// boolean — non-zero is true, zero is false (so `1`, `2`, `-1`, `1k` → true; `0`, `00`, `-0`, `0k`
			// → false). git parses this with `git_parse_int` (a signed **32-bit** int), so a value outside the
			// `i32` range (e.g. `2147483648`) is *not* a valid boolean, even though it is a valid integer.
			_ => match interpret_int(v) {
				Ok(n) if i32::try_from(n).is_ok() => Ok(n != 0),
				_ => Err(ConfigError::NotBool(v.to_owned())),
			},
		},
	}
}

fn interpret_int(value: &str) -> Result<i64, ConfigError> {
	// git parses this as `strtoimax` (base 0) followed by a *unit factor* on whatever immediately trails the
	// digits. `strtoimax` skips only *leading* ASCII whitespace, and `get_unit_factor` rejects any residue after
	// the number/suffix — so `0 k` (space before the multiplier) and a quoted trailing space (`"0 "`) are both
	// invalid. We therefore strip only git-compatible **leading** ASCII whitespace (never the trailing side, so
	// a significant quoted trailing space fails) and feed the stripped `digits` to `parse_c_integer` untrimmed
	// (so an interior space fails too). A bare unquoted value's trailing whitespace is already removed upstream
	// by the config parser; only a quoted one reaches here, and git rejects that.
	let leading = value.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
	let (digits, scale) = match leading.chars().last() {
		Some('k' | 'K') => (&leading[..leading.len() - 1], 1024),
		Some('m' | 'M') => (&leading[..leading.len() - 1], 1024 * 1024),
		Some('g' | 'G') => (&leading[..leading.len() - 1], 1024 * 1024 * 1024),
		_ => (leading, 1),
	};
	parse_c_integer(digits)
		.and_then(|n| n.checked_mul(scale))
		.ok_or_else(|| ConfigError::NotInt(value.to_owned()))
}

/// Parse an integer the way git does (C `strtoimax` with base 0): an optional sign, then base-0 auto-detect
/// — `0x`/`0X` hex, a leading `0` octal, otherwise decimal. So `0x10` = 16, `010` = 8, `08` is an invalid
/// octal (error), and `0`/`00`/`-0` = 0. `None` on any non-integer.
fn parse_c_integer(s: &str) -> Option<i64> {
	if s.is_empty() {
		return None;
	}
	let (negative, rest) = match s.strip_prefix('-') {
		Some(rest) => (true, rest),
		None => (false, s.strip_prefix('+').unwrap_or(s)),
	};
	let (radix, digits) =
		if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
			(16, hex)
		} else if rest.len() > 1 && rest.starts_with('0') {
			(8, &rest[1..])
		} else {
			(10, rest)
		};
	// Parse the magnitude *unsigned*, then apply the sign in `i128`, so `i64::MIN` (whose magnitude is one
	// above `i64::MAX`) round-trips — e.g. `-9223372036854775808` / `-0x8000000000000000`.
	let magnitude = u64::from_str_radix(digits, radix).ok()? as i128;
	i64::try_from(if negative { -magnitude } else { magnitude }).ok()
}

fn quote_value(value: &str) -> String {
	let needs_quote = value.is_empty()
		|| value.starts_with([' ', '\t'])
		|| value.ends_with([' ', '\t'])
		|| value.contains(['"', '\\', '#', ';', '\n', '\t']);
	if !needs_quote {
		return value.to_owned();
	}
	let mut out = String::from('"');
	for c in value.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\t' => out.push_str("\\t"),
			other => out.push(other),
		}
	}
	out.push('"');
	out
}

fn escape_subsection(sub: &str) -> String {
	let mut out = String::new();
	for c in sub.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			other => out.push(other),
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn int_suffixes() {
		assert_eq!(interpret_int("1").unwrap(), 1);
		assert_eq!(interpret_int("1k").unwrap(), 1024);
		// Leading whitespace is git-compatible (strtoimax skips it); a trailing space is not (get_unit_factor
		// rejects the residue), so the multiplier must directly abut the number.
		assert_eq!(interpret_int(" 2M").unwrap(), 2 * 1024 * 1024);
		assert!(interpret_int("2M ").is_err());
		assert!(interpret_int("nope").is_err());
	}

	#[test]
	fn bool_forms() {
		assert!(interpret_bool(None).unwrap());
		assert!(interpret_bool(Some("Yes")).unwrap());
		assert!(!interpret_bool(Some("off")).unwrap());
		assert!(!interpret_bool(Some("")).unwrap());
		assert!(interpret_bool(Some("maybe")).is_err());
	}

	#[test]
	fn subsections_lists_names_once_in_order() {
		let config = GitConfigSource::parse(
			"[remote \"origin\"]\n\turl = u1\n\tfetch = f1\n[remote \"upstream\"]\n\turl = u2\n[remote \"ghost\"]\n[core]\n\tbare = false\n",
		)
		.unwrap();
		// `ghost` has only a header (no variables), so — like git — it is not a remote.
		assert_eq!(config.subsections("remote"), vec!["origin", "upstream"]);
		// Section is case-folded; a section with no subsections yields nothing.
		assert_eq!(config.subsections("REMOTE"), vec!["origin", "upstream"]);
		assert!(config.subsections("core").is_empty());
	}

	#[test]
	fn variables_named_preserves_file_order_across_subsections() {
		// Interleaved subsections: the `insteadOf` variables must come back in file order (B then A),
		// not grouped by subsection — git relies on this to pick the first of tied rewrite rules.
		let config = GitConfigSource::parse(
			"[url \"A\"]\n\tpushInsteadOf = x:\n[url \"B\"]\n\tinsteadOf = xx:\n[url \"A\"]\n\tinsteadOf = xx:\n",
		)
		.unwrap();
		assert_eq!(
			config.variables_named("url", "insteadOf"),
			vec![(Some("B"), Some("xx:")), (Some("A"), Some("xx:"))],
		);
		assert_eq!(
			config.variables_named("url", "pushInsteadOf"),
			vec![(Some("A"), Some("x:"))],
		);
	}

	#[test]
	fn rename_subsection_moves_the_header_and_variables() {
		let mut config = GitConfigSource::parse(
			"[remote \"origin\"]\n\turl = u\n\tfetch = f\n[core]\n\tbare = false\n",
		)
		.unwrap();
		assert!(config.rename_subsection("remote", "origin", "upstream"));
		assert_eq!(config.subsections("remote"), vec!["upstream"]);
		assert_eq!(
			config.get_string("remote", Some("upstream"), "url"),
			Some("u")
		);
		assert!(config.get_string("remote", Some("origin"), "url").is_none());
		// The rendered header carries the new name, and unrelated sections survive.
		assert!(config.render().contains("[remote \"upstream\"]"));
		assert_eq!(config.get_string("core", None, "bare"), Some("false"));
		// Renaming an absent subsection reports nothing.
		assert!(!config.rename_subsection("remote", "origin", "x"));
	}

	#[test]
	fn remove_subsection_drops_the_whole_remote() {
		let mut config = GitConfigSource::parse(
			"[core]\n\tbare = false\n[remote \"origin\"]\n\turl = u\n\tfetch = a\n\tfetch = b\n[branch \"main\"]\n\tremote = origin\n",
		)
		.unwrap();
		assert!(config.remove_subsection("remote", "origin"));
		assert_eq!(config.subsections("remote"), Vec::<&str>::new());
		assert!(config.get_string("remote", Some("origin"), "url").is_none());
		// Unrelated sections survive.
		assert_eq!(config.get_string("core", None, "bare"), Some("false"));
		assert_eq!(
			config.get_string("branch", Some("main"), "remote"),
			Some("origin")
		);
		// Removing an absent subsection reports nothing removed.
		assert!(!config.remove_subsection("remote", "origin"));
	}

	#[test]
	fn set_get_render_round_trip() {
		let mut config = GitConfigSource::new();
		config
			.set("core", None, "repositoryformatversion", "1")
			.unwrap();
		config.set("core", None, "bare", "false").unwrap();
		config
			.set("extensions", None, "objectFormat", "sha256")
			.unwrap();

		assert_eq!(
			config
				.get_int("core", None, "repositoryformatversion")
				.unwrap(),
			Some(1)
		);
		// Key lookup is case-insensitive.
		assert_eq!(
			config.get_string("Extensions", None, "objectformat"),
			Some("sha256")
		);

		let reparsed = GitConfigSource::parse(&config.render()).unwrap();
		assert_eq!(reparsed, config);
	}

	#[test]
	fn set_existing_value_preserves_comments_and_layout() {
		let text = concat!(
			"# top comment\n",
			"[user]\n",
			"\tname = Old Name   # inline note\n",
			"\temail = a@example.com\n",
		);
		let mut config = GitConfigSource::parse(text).unwrap();
		config.set("user", None, "name", "New Name").unwrap();

		assert_eq!(config.get_string("user", None, "name"), Some("New Name"));
		// Only the value changed: the comment, the other variable, and the inline note all survive.
		assert_eq!(
			config.render(),
			concat!(
				"# top comment\n",
				"[user]\n",
				"\tname = New Name   # inline note\n",
				"\temail = a@example.com\n",
			)
		);
	}

	#[test]
	fn get_bool_accepts_gits_numeric_boolean_grammar() {
		let text = concat!(
			"[core]\n",
			"\tone = 1\n\ttwo = 2\n\tneg = -1\n\tkilo = 1k\n",
			"\tzero = 0\n\tzerok = 0k\n\tdblzero = 00\n\tnegzero = -0\n",
			"\ttrue = true\n\toff = off\n\tempty =\n\tbad = banana\n",
		);
		let config = GitConfigSource::parse(text).unwrap();
		let b = |k: &str| config.get_bool("core", None, k);
		// Non-zero numerics (incl. suffixed/negative) are true.
		for k in ["one", "two", "neg", "kilo"] {
			assert_eq!(b(k).unwrap(), Some(true), "{k} should be true");
		}
		// Zero spellings are false.
		for k in ["zero", "zerok", "dblzero", "negzero", "off", "empty"] {
			assert_eq!(b(k).unwrap(), Some(false), "{k} should be false");
		}
		assert_eq!(b("true").unwrap(), Some(true));
		// A non-numeric, non-keyword value is still not a boolean.
		assert!(b("bad").is_err(), "a non-boolean value must error");
	}

	#[test]
	fn get_bool_uses_gits_base_0_integer_grammar() {
		// git parses config integers with C base-0: 0x hex, leading-0 octal, else decimal.
		let text = concat!(
			"[core]\n",
			"\thex0 = 0x0\n\thexff = 0xff\n\toct = 010\n\toctzero = 00\n\tbadoct = 08\n",
		);
		let config = GitConfigSource::parse(text).unwrap();
		let b = |k: &str| config.get_bool("core", None, k);
		assert_eq!(b("hex0").unwrap(), Some(false), "0x0 is zero → false");
		assert_eq!(b("hexff").unwrap(), Some(true), "0xff is non-zero → true");
		assert_eq!(b("oct").unwrap(), Some(true), "010 is octal 8 → true");
		assert_eq!(b("octzero").unwrap(), Some(false), "00 is zero → false");
		assert!(
			b("badoct").is_err(),
			"08 is an invalid octal, not a boolean"
		);
	}

	#[test]
	fn numeric_boolean_is_bounded_to_i32_but_int_keeps_i64_range() {
		let text = concat!(
			"[core]\n",
			"\tbigbool = 2147483648\n", // 2^31 — a valid i64, but out of git's boolean int32 range
			"\tmin = -9223372036854775808\n", // i64::MIN, decimal
			"\thexmin = -0x8000000000000000\n",
			"\tmax = 9223372036854775807\n", // i64::MAX
		);
		let config = GitConfigSource::parse(text).unwrap();
		// A numeric boolean beyond i32 is not a boolean (git parses booleans as a signed 32-bit int).
		assert!(config.get_bool("core", None, "bigbool").is_err());
		// …but get_int keeps git's full signed-64 range, including i64::MIN (previously rejected by
		// sign-stripping) via both decimal and hex spellings, and i64::MAX.
		assert_eq!(config.get_int("core", None, "min").unwrap(), Some(i64::MIN));
		assert_eq!(
			config.get_int("core", None, "hexmin").unwrap(),
			Some(i64::MIN)
		);
		assert_eq!(config.get_int("core", None, "max").unwrap(), Some(i64::MAX));
	}

	#[test]
	fn integer_rejects_whitespace_before_a_multiplier_suffix() {
		// git's `get_unit_factor` requires the multiplier to directly abut the number: `0 k` (a space between the
		// digits and the `k`) is not a valid integer, so it is not a valid boolean either. This matters for
		// fail-closed gates like `core.fileMode` — a spurious `false` there would let removal delete a modified
		// checkout — so a space-before-multiplier value must error, not parse as zero/false.
		let text = concat!(
			"[core]\n",
			"\tspaced = 0 k\n",
			"\tspacedhi = 1 m\n",
			"\ttrailing = \"0 \"\n", // quoted trailing space is significant — git rejects it, must not be false
			"\tleading = \" 0\"\n",  // git's strtoimax skips leading whitespace — accepted as 0
			"\tleadingpos = \" 5\"\n",
			"\ttight = 1k\n", // the valid form still works
		);
		let config = GitConfigSource::parse(text).unwrap();
		assert!(config.get_int("core", None, "spaced").is_err());
		assert!(config.get_int("core", None, "spacedhi").is_err());
		assert!(config.get_bool("core", None, "spaced").is_err());
		// A quoted trailing space is NOT trimmed away — the value stays invalid (fail closed), never false.
		assert!(config.get_int("core", None, "trailing").is_err());
		assert!(config.get_bool("core", None, "trailing").is_err());
		// Leading whitespace is git-compatible (strtoimax skips it).
		assert_eq!(config.get_int("core", None, "leading").unwrap(), Some(0));
		assert_eq!(
			config.get_bool("core", None, "leading").unwrap(),
			Some(false)
		);
		assert_eq!(config.get_int("core", None, "leadingpos").unwrap(), Some(5));
		assert_eq!(config.get_int("core", None, "tight").unwrap(), Some(1024));
		assert_eq!(config.get_bool("core", None, "tight").unwrap(), Some(true));
	}

	#[test]
	fn set_new_key_inserts_into_existing_section() {
		let text = "[user]\n\tname = A\n# trailing comment\n";
		let mut config = GitConfigSource::parse(text).unwrap();
		config.set("user", None, "email", "a@example.com").unwrap();

		// Inserted after the last variable of the section, before the trailing comment.
		assert_eq!(
			config.render(),
			"[user]\n\tname = A\n\temail = a@example.com\n# trailing comment\n"
		);
	}

	#[test]
	fn set_new_section_appends_at_end() {
		let text = "[user]\n\tname = A\n";
		let mut config = GitConfigSource::parse(text).unwrap();
		config
			.set("remote", Some("origin"), "url", "http://example/x")
			.unwrap();

		assert_eq!(
			config.render(),
			"[user]\n\tname = A\n[remote \"origin\"]\n\turl = http://example/x\n"
		);
	}

	#[test]
	fn add_appends_without_disturbing_layout() {
		let text = "[remote \"o\"]\n\tfetch = one   # keep me\n";
		let mut config = GitConfigSource::parse(text).unwrap();
		config.add("remote", Some("o"), "fetch", Some("two"));

		assert_eq!(
			config.get_all("remote", Some("o"), "fetch"),
			vec!["one", "two"]
		);
		assert_eq!(
			config.render(),
			"[remote \"o\"]\n\tfetch = one   # keep me\n\tfetch = two\n"
		);
	}

	#[test]
	fn append_preserves_strict_insertion_order_unlike_add() {
		// `add` groups a repeat back into the earlier `[x]` block; `append` keeps each entry as its own
		// block in insertion order — what command-scope config needs so an interleaved directive stays put.
		let mut added = GitConfigSource::new();
		added.add("x", None, "v", Some("a"));
		added.add("y", None, "w", Some("mid"));
		added.add("x", None, "v", Some("b"));
		// `add` reorders: the second `x.v` joins the first, so `y.w` ends up last in file order.
		assert_eq!(
			added.entries().map(|(k, _)| k).collect::<Vec<_>>(),
			["x.v", "x.v", "y.w"]
		);

		let mut appended = GitConfigSource::new();
		appended.append("x", None, "v", Some("a"));
		appended.append("y", None, "w", Some("mid"));
		appended.append("x", None, "v", Some("b"));
		// `append` keeps command-line order: a, mid, b.
		assert_eq!(
			appended.entries().collect::<Vec<_>>(),
			[
				("x.v".to_owned(), Some("a")),
				("y.w".to_owned(), Some("mid")),
				("x.v".to_owned(), Some("b")),
			]
		);
		// Multi-value and last-wins are still correct across the ungrouped blocks.
		assert_eq!(appended.get_all("x", None, "v"), vec!["a", "b"]);
		assert_eq!(appended.get_string("x", None, "v"), Some("b"));
	}

	#[test]
	fn unset_removes_only_the_target_line() {
		let text = concat!(
			"[user]\n",
			"# keep this comment\n",
			"\tname = A\n",
			"\temail = a@example.com\n",
		);
		let mut config = GitConfigSource::parse(text).unwrap();
		assert!(config.unset("user", None, "name"));

		// The variable line is gone; the comment, the section header, and the sibling survive.
		assert_eq!(
			config.render(),
			"[user]\n# keep this comment\n\temail = a@example.com\n"
		);
		assert!(!config.unset("user", None, "name"));
	}

	#[test]
	fn set_value_needing_quotes_is_escaped_in_place() {
		let text = "[core]\n\tpager = less\n";
		let mut config = GitConfigSource::parse(text).unwrap();
		config.set("core", None, "pager", "less # paged").unwrap();

		assert_eq!(
			config.get_string("core", None, "pager"),
			Some("less # paged")
		);
		assert_eq!(config.render(), "[core]\n\tpager = \"less # paged\"\n");
	}

	#[test]
	fn set_in_place_preserves_crlf_line_endings() {
		let text = "[user]\r\n\tname = Old\r\n";
		let mut config = GitConfigSource::parse(text).unwrap();
		config.set("user", None, "name", "New").unwrap();

		assert_eq!(config.get_string("user", None, "name"), Some("New"));
		// Only the value text changed; the CRLF ending is untouched.
		assert_eq!(config.render(), "[user]\r\n\tname = New\r\n");
	}

	#[test]
	fn insert_adds_a_newline_when_the_file_lacks_a_final_one() {
		// No trailing newline on the last line.
		let mut config = GitConfigSource::parse("[user]\n\tname = Alice").unwrap();
		config.set("user", None, "email", "a@example.com").unwrap();
		// The new key lands on its own line, not glued onto `Alice`.
		assert_eq!(
			config.render(),
			"[user]\n\tname = Alice\n\temail = a@example.com\n"
		);

		// Same when a brand-new section has to be appended.
		let mut config = GitConfigSource::parse("[core]\n\trepositoryformatversion = 1").unwrap();
		config.set("user", None, "name", "Alice").unwrap();
		assert_eq!(
			config.render(),
			"[core]\n\trepositoryformatversion = 1\n[user]\n\tname = Alice\n"
		);
	}

	#[test]
	fn set_refuses_to_collapse_a_multi_valued_variable() {
		let text = "[remote \"o\"]\n\tfetch = one\n\tfetch = two\n";
		let mut config = GitConfigSource::parse(text).unwrap();

		let err = config
			.set("remote", Some("o"), "fetch", "three")
			.unwrap_err();
		assert!(matches!(err, ConfigError::MultipleValues(_)), "{err:?}");
		// The config is left untouched, so no values are lost.
		assert_eq!(config.render(), text);
		assert_eq!(
			config.get_all("remote", Some("o"), "fetch"),
			vec!["one", "two"]
		);
	}

	#[test]
	fn replace_all_collapses_multiple_values_to_one() {
		let text = concat!(
			"[remote \"o\"]\n",
			"\tfetch = one   # note\n",
			"# a comment between values\n",
			"\tfetch = two\n",
		);
		let mut config = GitConfigSource::parse(text).unwrap();
		config.replace_all("remote", Some("o"), "fetch", "three");

		assert_eq!(config.get_all("remote", Some("o"), "fetch"), vec!["three"]);
		// The first occurrence is edited in place (its inline note kept) and the second is removed;
		// the unrelated comment survives.
		assert_eq!(
			config.render(),
			concat!(
				"[remote \"o\"]\n",
				"\tfetch = three   # note\n",
				"# a comment between values\n",
			)
		);
	}

	#[test]
	fn replace_all_inserts_when_absent() {
		let mut config = GitConfigSource::parse("[user]\n\tname = A\n").unwrap();
		config.replace_all("user", None, "email", "a@example.com");
		assert_eq!(
			config.render(),
			"[user]\n\tname = A\n\temail = a@example.com\n"
		);
	}
}
