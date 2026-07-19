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
  - `onbranch:<pat>` — matched against the **short current branch** name (no `refs/heads/`), same wildmatch
    + trailing-`/`→`**` rule as gitdir. **No match when detached or bare** (no current branch).
  - `hasconfig:<var-glob>:<value-glob>` (e.g. `hasconfig:remote.*.url:https://example.com/**`) — matches
    when a variable whose full name matches `<var-glob>` has a value matching `<value-glob>`, evaluated
    against **the configuration parsed so far** (lower-precedence layers + earlier-in-traversal content).

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
    pub home: Option<&'a Path>,        // $HOME, for ~/ expansion (None => ~/ include is skipped)
    pub gitdir: Option<&'a Path>,      // real absolute gitdir, for gitdir: (None => gitdir: never matches)
    pub branch: Option<&'a str>,       // short current branch, for onbranch: (None => onbranch: never matches)
    // hasconfig: is evaluated against the config-so-far the driver threads in (see slice 2).
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
- The driver owns: the actual reads, and supplying `home`/`gitdir`/`branch`/config-so-far.

Consumers and where the driver lives:
- **gta-core** (`git_config.rs`): the ambient-file reader (`read_sources`/`read_file`) — the choke point
  where every layer becomes a `GitConfigSource`. gitdir + branch come from the `effective_config_*` callers.
- **wasm component**: a resolver over the `FileStore` capability, so in-component reads expand includes.
- The engine's `Repository::read_config` is already covered via the CLI-injected merged `effective_config`.

## Slices (each its own worktree/branch/codex/merge)

1. **Engine core — `include` + `includeIf gitdir`.** `IncludeResolver` + `IncludeContext`, inline splice,
   path resolution (`~/`/relative/absolute), gitdir wildmatch, depth-10 fatal, missing-file skip. Pure unit
   tests against the probed cases. No consumer wiring.
2. **Engine — `onbranch` + `hasconfig`.** Branch in the context; hasconfig against a threaded config-so-far;
   wildmatch shared with gitdir.
3. **gta-core wiring.** Filesystem resolver, thread gitdir + branch into the config-load path, shared
   `expand_tilde`. Oracle tests vs stock git (per-directory identity, onbranch, hasconfig).
4. **wasm component wiring.** Resolver over the `FileStore` capability; component tests.

## Wildmatch note

git's `gitdir:`/`onbranch:` use its `wildmatch` with `WM_PATHNAME` (`*` stops at `/`, `**` crosses `/`).
gitana has no wildmatch yet; slice 1 introduces a small internal matcher covering `*`, `**`, `?`, bracket
expressions (`[a-z]`, sets, `[!…]`/`[^…]` negation, and POSIX `[[:class:]]`), and literal segments, with the
trailing-`/`→`**` and no-slash→`**/` preprocessing. It is an O(tokens × text) dynamic program (not recursive
backtracking, which is exponential on adversarial patterns), and config-internal (not the `.gitignore` matcher
in `gitana-worktree`, which has different anchoring rules). An unterminated `[` makes the whole pattern
non-matching, matching git's abort behaviour.

## Deferred

Known git-fidelity gaps left for later slices, each with a code note at its site:

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
