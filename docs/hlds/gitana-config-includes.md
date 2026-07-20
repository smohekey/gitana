# HLD: `include` / `includeIf` expansion in `gitana-config`

Closes the "`[include]` / `includeIf` not expanded" gap tracked in
`docs/hlds/gitana-config-followups.md`. git expands includes while parsing; gitana did not, anywhere, so a
value set only via an include was invisible to every config read (notably per-directory identities via
`includeIf "gitdir:…"`).

## git semantics (probed against stock git 2.50.1)

- **Inline expansion.** `[include] path = X` expands X's contents *at the directive's position*. Ordering is
  preserved and last-value-wins: a value before the include is overridden by the include; a value after the
  include overrides it. So expansion is an **inline splice within the one file**, never a separate layer.
- **Path resolution** of the `path` value: leading `~/` → `$HOME`; a relative path → relative to the
  **including file's directory**; an absolute path as-is.
- **Recursion + depth cap.** Included files may include further. git caps at **depth 10** and emits a
  **fatal error** (`exceeded maximum include depth (10)`) on exceeding it (also how it breaks cycles).
- **Missing include file** is silently ignored (git does not error on an absent `include.path`).
- **Conditions** (`[includeIf "<cond>"] path = X`), included only when `<cond>` matches:
  - `gitdir:<pat>` / `gitdir/i:<pat>` (case-insensitive) — matched against the **real (symlink-resolved)
    absolute gitdir path**, with git's pattern rules:
    - leading `~/` → `$HOME`; leading `./` → relative to the including file's dir; any other **relative**
      (non-absolute) pattern is prefixed with `**/` — regardless of whether it contains a slash (probed:
      `gitdir:b/c/` and `gitdir:c/` both match anywhere, so it is *all* relative patterns, not just
      slash-free ones);
    - a **trailing `/`** appends `**`;
    - `**` matches any number of path components (including none); `*` matches within a component.
    - An exact gitdir path *with* a trailing slash does **not** match the gitdir itself (the appended `**`
      needs trailing content) — users point at the parent dir.
  - `onbranch:<pat>` — matched against the **short current branch** name (no `refs/heads/`) with git's
    `WM_PATHNAME` wildmatch. Preprocessing is **only** the trailing-`/`→`**` rule — unlike gitdir it does
    **not** prepend `**/` (probed 2.50.1: `onbranch:foo` does *not* match branch `feature/foo`, while
    `feature/*`, `feature/**`, `feature/`, and the exact `feature/foo` all match). Matching is
    case-sensitive. The branch is read straight off a **symbolic HEAD**, so it is present for an unborn
    branch and a **bare repo** too (probed: a bare repo with `HEAD -> refs/heads/main` matches
    `onbranch:main`); `None` — hence never-matching — is reserved for a **detached** HEAD.
  - `hasconfig:remote.*.url:<value-glob>` — **the only `hasconfig` form git implements** (corrected against
    the original design's general `<var-glob>:<value-glob>` — 2.50.1 probes show git recognises *only* the
    literal prefix `hasconfig:remote.*.url:`; `hasconfig:some.key:…`, `remote.?.url:…`, `remote.*.URL:…`, and
    `remote.*.pushurl:…` all fall through to "unknown conditional → false"). It matches when *any*
    `remote.<name>.url` (a subsection is required; bare `[remote] url` is not collected) anywhere in the
    **whole effective config** has a value matching `<value-glob>` under a plain anchored `WM_PATHNAME`
    wildmatch (case-sensitive; `github.com/*` does not span the path, `github.com/**` does). "Whole config"
    is **not** "config so far": probed URLs set *after* the directive, in a *different layer* (global vs
    local), and by an *earlier include* all match — git resolves this with a separate pre-scan of the entire
    config for remote URLs. A file pulled in (directly or indirectly) by such a directive **may not itself
    set a `remote.<name>.url`** — git fatals (`remote URLs cannot be configured in file directly or
    indirectly included by includeIf.hasconfig:remote.*.url`), even when the condition does not match.

## Architecture

`gitana-config` is I/O-free and wasm-pure (its only dep is `thiserror`). To keep it so while owning the
expansion logic (only the crate has the ordered-element access needed for a faithful inline splice), the
crate exposes an expansion driven by a **caller-supplied async resolver + context**:

```rust
pub trait IncludeResolver {
    // Read an include target's text, or None if absent (absent = silently skipped, per git).
    async fn read(&self, path: &Path) -> Result<Option<String>, ConfigError>;
}

pub struct IncludeContext<'a> {
    pub home: Option<&'a Path>,          // $HOME, for ~/ expansion (None => ~/ include is skipped)
    pub gitdir: Option<&'a Path>,        // real absolute gitdir, for gitdir: (None => gitdir: never matches)
    pub branch: Option<&'a str>,         // short current branch, for onbranch: (None => never matches)
    pub remote_urls: Option<&'a [&'a str]>, // every remote.<name>.url across the whole effective config,
                                         // for hasconfig:remote.*.url: (driver-collected; None/empty => never matches)
}

impl GitConfigSource {
    // `dir` is the including file's directory (for relative paths). Expands in place, recursively.
    pub async fn expand_includes<R: IncludeResolver>(
        &mut self, dir: &Path, ctx: &IncludeContext<'_>, resolver: &R,
    ) -> Result<(), ConfigError>;
}
```

- **Async resolver** so both gta-core (`tokio::fs`) and the wasm component (async `FileStore` capability)
  back it without an API rev. The crate uses `async`/`await` only — no runtime dependency, wasm-pure intact.
- The crate owns: directive detection (`section == "include" | "includeif"`, `name == "path"`), condition
  matching (gitdir/onbranch/hasconfig wildmatch), path resolution, recursion + depth-10 fatal, and the inline
  element splice (replace the directive `Element::Variable` with the expanded included file's elements).
- The driver owns: the actual reads, and supplying `home`/`gitdir`/`branch`/`remote_urls`. Because
  `hasconfig` spans the whole config, only the driver (which sees every layer) can collect the remote URLs;
  the engine merely wildmatches the condition's value-glob against the supplied list. The paradox guard's
  no-match arm (git fatals there too) likewise needs the driver's forced pre-scan — the engine covers only
  the matched arm it actually walks.

Consumers and where the driver lives:
- **gta-core** (`git_config.rs`): the ambient-file reader (`read_sources`/`read_file`) — the choke point
  where every layer becomes a `GitConfigSource`. gitdir + branch come from the `effective_config_*` callers.
- **wasm component**: a resolver over the `FileStore` capability, so in-component reads expand includes.
- The engine's `Repository::read_config` is already covered via the CLI-injected merged `effective_config`.

## Slices (each its own worktree/branch/codex/merge)

1. **Engine core — `include` + `includeIf gitdir`.** `IncludeResolver` + `IncludeContext`, inline splice,
   path resolution (`~/`/relative/absolute), gitdir wildmatch, depth-10 fatal, missing-file skip. Pure unit
   tests against the probed cases. No consumer wiring.
2. **Engine — `onbranch` + `hasconfig`.** Branch matched against `ctx.branch` (trailing-`/`→`**` only, no
   `**/` prefix); `hasconfig:remote.*.url:` recognised as a literal prefix and its value-glob wildmatched
   against the driver-supplied `ctx.remote_urls`; the paradox guard (a hasconfig-included file setting a
   `remote.<name>.url` is fatal) enforced on the matched path. The cross-layer URL collection and the
   no-match arm of the guard are slice 3.
3. **gta-core wiring.** Filesystem resolver, thread gitdir + branch into the config-load path, collect
   `remote.*.url` across all layers (git's pre-scan, forbidding remote URLs inside hasconfig-included files)
   into `ctx.remote_urls`, shared `expand_tilde`. Oracle tests vs stock git (per-directory identity,
   onbranch, hasconfig).
4. **wasm component wiring.** Resolver over the `FileStore` capability; component tests.

## Wildmatch note

git's `gitdir:`/`onbranch:`/`hasconfig:` use its `wildmatch` with `WM_PATHNAME` (`*` stops at `/`, `**`
crosses `/`). gitana has no wildmatch yet; slice 1 introduces a small internal matcher covering `*`, `**`,
`?`, bracket expressions (`[a-z]`, sets, `[!…]`/`[^…]` negation, and POSIX `[[:class:]]`), and literal
segments, with the trailing-`/`→`**` and (gitdir-only) no-slash→`**/` preprocessing. It is an O(tokens × text)
dynamic program (not recursive backtracking, which is exponential on adversarial patterns) kept to **O(text)
space** (rolling rows, so a large `hasconfig` glob/URL cannot exhaust memory; no length cap — git matches long
patterns), and config-internal (not the `.gitignore` matcher in `gitana-worktree`, which has different
anchoring rules).

Malformed-construct handling matches git's `wildmatch` byte-for-byte (verified against 2.50.1 source +
probes): an **unterminated `[`**, a **terminated-but-unknown POSIX class** (`[[:bogus:]]`), and a **trailing
backslash** each abort the whole match (`WM_ABORT_ALL` → matches nothing); but a `[:` with **no `:]`
terminator** is *not* malformed — the `[` is an ordinary set member (`[[:abc]` = the set `{[ : a b c}`). A
**descending range** matches only its low endpoint (`[b-a]` matches `b`), because git reads that endpoint as a
literal member before it sees the `-`.

## Deferred

Known git-fidelity gaps left for later slices, each with a code note at its site:

- **`hasconfig` URL-paradox vs. a later *syntax error* in the same included file.** git parses config
  incrementally, so a forbidden `remote.<name>.url` in a hasconfig-included file fatals with the paradox
  error even when a *syntax error* follows it later in the file. gitana parses each included file
  atomically before the positional guard walks it, so a URL lexically preceding a syntax error surfaces
  the parse error instead (both still fail closed — only the message differs). The positional guard is
  faithful for a later *include* (that is exercised); matching git for a later *parse error* would need an
  incremental parser, which is disproportionate for this exotic broken-config case. (P3, probed 2.50.1.)
- **`./` canonicalization through `..`/symlinks (slice-3 driver responsibility).** `includeIf "gitdir:./…"`
  in a config file reached via a `..`-relative or symlinked path is matched against the **lexical** parent of
  the including file, which can differ from its realpath. git resolves the including file to a real path first.
  The pure `gitana-config` crate performs no filesystem I/O and so cannot realpath; the gta-core / wasm driver
  (slice 3) must pass an already-canonicalized including-file directory into `IncludeContext`/`expand_includes`.
- **Mutating an include directive on an already-expanded source.** `set`/`unset`/`replace_all`/
  `remove_subsection`/`rename_subsection` on an `include`/`includeif` directive do **not** re-run the include,
  so the previously spliced `Included` entries remain as stale reads until the next expansion. The intended
  consumer design avoids this — the *writable* source stays raw (unexpanded) and reads use separate expanded
  copies — so a caller must not both expand and mutate include directives on the same source. `expand_includes`
  is idempotent, so re-calling it after an include change is the supported refresh path.
- **Windows path separators** (backslash, verbatim `\\?\` prefixes) in gitdir matching — matching assumes
  git's slash-form paths, as the Unix/wasm drivers supply.
- **Escaped separator `\/` after a globstar** is not treated as a path-segment boundary (exotic).
- **`~user/` include paths** need a passwd lookup the pure crate cannot perform; they fail closed
  (`IncludeUserTildeUnsupported`) and are a native-driver follow-up.
- **Non-UTF-8 native paths** degrade lossily in gitdir matching (the driver supplies real UTF-8 paths).
