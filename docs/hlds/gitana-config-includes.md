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

    **The pre-scan and its forbid are broader than "hasconfig-included files" (re-probed 2.50.1, slice 3).**
    git's pre-scan (`populate_remote_urls`) runs the *whole* config with `unconditional_remote_url` — so
    every `hasconfig` condition is forced true — and, crucially, it arms `forbid_remote_url` when entering
    **any matched `includeIf` subtree**, not only a `hasconfig` one. So a **matching `gitdir:` / `onbranch:`**
    include whose target sets a `remote.<name>.url` is *also* the paradox and fatals — **but only when a
    `hasconfig` directive exists somewhere to trigger the pre-scan** (with no `hasconfig` anywhere, the same
    matched-`gitdir` URL is perfectly fine). The trigger, the collection, and the forbid are all
    **whole-config / cross-layer** (a `hasconfig` in the global layer forbids a URL in a matched include of
    the local layer, and vice-versa); URLs are collected only from the **top level and plain `[include]`s**
    (a URL reachable only through an `includeIf` is the paradox, never a collected URL); and even a
    *non-`path`* `hasconfig` key triggers the pre-scan (git evaluates the condition before checking the key).
    Finally, an explicitly **scoped** single-file read (`git config --global`/`--system`/`--local`) does
    **not** expand includes at all — only a *merged* read does.

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
- The crate owns two operations, both driven by the resolver + context:
  - **`expand_includes`** — directive detection (`section == "include" | "includeif"`, `name == "path"`),
    condition matching (gitdir/onbranch/hasconfig wildmatch against `ctx.remote_urls`), path resolution,
    recursion + depth-10 fatal, and the inline element splice. It does **not** enforce the paradox (see below).
  - **`scan_remote_urls`** (slice 3) — git's `populate_remote_urls`: walk the include graph with `hasconfig`
    forced true, returning a `RemoteUrlScan { urls, has_hasconfig, forbidden_url }`. It collects
    `remote.<name>.url` from the top level and plain `[include]`s, records whether any matched-`includeIf`
    subtree set a URL (`forbidden_url`, *not* fatalled here so the driver can combine it cross-layer), and
    whether any `hasconfig` directive was reached (`has_hasconfig`, the trigger).
- The driver owns: the actual reads, supplying `home`/`gitdir`/`branch`, and **orchestrating the pre-scan
  across layers**. It runs `scan_remote_urls` on every layer, unions the URLs into `ctx.remote_urls`, and —
  because git's paradox is whole-config — fatals with `HasconfigIncludeSetsRemoteUrl` exactly when
  `has_hasconfig && forbidden_url` hold across the combined layers. Only then does it call `expand_includes`
  on each layer. So the pre-scan is the **single authority** for the paradox (every arm, every layer);
  `expand_includes` carries none of it (it always runs after a passing pre-scan). This split is *why*
  slice 2's matched-arm enforcement was removed from `expand_includes` in slice 3.

Consumers and where the driver lives:
- **gta-core** (`git_config.rs`): the merged-config assemblers (`effective_config_at`,
  `effective_config_for_worktree`, `ambient_effective`) build the ordered `(source, dir)` layers, run the
  cross-layer `expand_layers` (pre-scan + paradox + expand), and assemble the `GitConfig`. `gitdir` is the
  canonicalized real git dir; `branch` is the short symbolic-`HEAD` target; each layer's `dir` is the
  canonicalized parent of its file. A **scoped** single-file read (`--global`/`--system`/`--local`) skips
  expansion, matching git.
- **wasm component**: a resolver over the `FileStore` capability, so in-component reads expand includes.
- The engine's `Repository::read_config` is already covered via the CLI-injected merged effective config.

## Slices (each its own worktree/branch/codex/merge)

1. **Engine core — `include` + `includeIf gitdir`.** `IncludeResolver` + `IncludeContext`, inline splice,
   path resolution (`~/`/relative/absolute), gitdir wildmatch, depth-10 fatal, missing-file skip. Pure unit
   tests against the probed cases. No consumer wiring.
2. **Engine — `onbranch` + `hasconfig`.** Branch matched against `ctx.branch` (trailing-`/`→`**` only, no
   `**/` prefix); `hasconfig:remote.*.url:` recognised as a literal prefix and its value-glob wildmatched
   against the driver-supplied `ctx.remote_urls`; the paradox guard (a hasconfig-included file setting a
   `remote.<name>.url` is fatal) enforced on the matched path. The cross-layer URL collection and the
   no-match arm of the guard are slice 3.
3. **gta-core wiring — DONE.** Added the engine's `scan_remote_urls` pre-scan (`RemoteUrlScan`) and removed
   the paradox enforcement from `expand_includes` (the pre-scan is now the sole authority). Added a
   `tokio::fs` `FsIncludeResolver`; reworked the merged assemblers (`effective_config_at`,
   `effective_config_for_worktree`, `ambient_effective`) onto ordered `(source, dir)` layers driven through
   `expand_layers` (cross-layer pre-scan → paradox gate → expand). Threads the canonicalized real gitdir and
   the short symbolic-`HEAD` branch; passes each layer **both** its lexical dir (relative `include.path`) and
   its realpath'd dir (`gitdir:./`), and `IncludeResolver::read` returns each file's canonical path so nested
   includes split the same way (closing the slice-1 `./` realpath gap and matching git's symlink handling).
   Command-scope (`-c`/`GIT_CONFIG_*`) config is threaded through the pre-scan + expansion (so a `-c
   include.path` expands and a `-c remote.url` feeds `hasconfig`) then overlaid so writes stay local. Scoped
   single-file reads deliberately do **not** expand, matching git. `open_generic` feeds
   its result to `Repository::set_effective_config`; identity/`gta config` reuse the installed config. Oracle
   tests vs stock git: per-directory identity (gitdir), onbranch, hasconfig match, the no-match paradox, and
   the matched-`gitdir`-subtree forbid (present only with a `hasconfig`). No shared `expand_tilde` was
   needed — the engine already interpolates `~/` in include paths and gitdir patterns via `ctx.home`.
   **Signing reads routed through the merged config.** `signer.rs` read the raw *local* `.git/config` for
   `commit.gpgSign` / `tag.gpgSign`, `gpg.format`, and `user.signingkey` — so a signing switch or key set in
   global (or an *included*) file was invisible to signing while `gta config` reported it, and gta would
   silently write an **unsigned** commit where git signs. All four sites now read `Repository::effective_config`
   (the same merged, include-expanded stack), which falls back to the local file when no merged view is
   installed. Oracle test: `commit.gpgSign`+`gpg.format`+`user.signingkey` delivered via an `[include]` in
   global config with a missing key makes both gta and git refuse the commit. (This closes a *pre-existing*
   local-only-signing gap the includes work widened; it is not specific to includes.)
4. **wasm component wiring — DONE.** A `FileStoreIncludeResolver` over the component's `FileStore` capability
   (`gitana-repo-component`, `ops/include_resolver.rs`), resolving include targets *relative to the store root*
   (the path-less descriptor) with git's **filesystem** semantics as far as the capability allows: an absolute
   path escapes → skipped; a `..` is admitted only when the accumulated prefix is an existing directory
   (probed by `is_dir`), so a `..` through a **missing or non-directory** prefix — or one climbing above the
   root — skips the include exactly as git skips the resulting `ENOENT`/`ENOTDIR` (probed 2.50.1: `missing/../x`
   is skipped, `sub/../x` with `sub` present is read); a **directory** target aborts (git fatals `Is a
   directory`) while any other read failure skips. (Residual: git resolves a *symlinked* prefix through the
   link, which the path-less store cannot — no `realpath` — so a `..` after a symlink is the one divergence;
   realistic include paths do not use it.) The common `config` and its relative includes are read from the
   **common** store (`WorktreeFileStore::common()`) — git resolves them against the common dir, so routing an
   include named like a per-worktree file through the per-path routing would send it to the wrong directory.
   Under `extensions.worktreeConfig` (read from the *unexpanded* common config, git's rule), the per-worktree
   `<git-dir>/config.worktree` is layered **above** it (`local < config.worktree`), read from the per-worktree
   store (`WorktreeFileStore::worktree()`) so *its* relative includes resolve there; HEAD (for `onbranch:`) is
   likewise per-worktree. The whole-config `hasconfig` pre-scan spans both layers.

   Config is expanded **once at open** — pre-scan (`scan_remote_urls`) *before* `expand_includes`, per layer, as
   gta-core does — and installed via `set_effective_config`, so **every** consumer honours includes:
   `pack.packSizeLimit` (`repack`), `remote.origin.fetch`/`tagOpt` (`fetch`, via `gitana_porcelain`), and
   `core.logAllRefUpdates` (ref writes) all read the installed effective config. `read-config` stays the **raw**
   file (git's plumbing contract, host-parsed). `init` installs too (it is idempotent and may reopen a repo that
   already has includes). A structurally bad include (cycle/paradox/directory/`~`-no-home) or a malformed
   `extensions.worktreeConfig` aborts the *open*; a bad *value* surfaces at its consumer (a malformed
   `pack.packSizeLimit` fails `repack`). `onbranch:` resolves from HEAD; `hasconfig:remote.*.url:` from the
   whole-config pre-scan.

   Inherent capability limits (documented divergences): **`gitdir:` conditions never match** (no gitdir path in
   a descriptor); there is no global/system/`-c` layer or `$PWD` (so no `gitdir_absolute` candidate); a
   `~`/`~user` include is **fatal** (no `$HOME`; engine `IncludeTildeNoHome`), exactly as git with `HOME` unset
   (probed 2.50.1). And, where the `FileStore` — designed for git's object/ref storage — cannot express git's
   config-file *syscall* semantics: (a) config is a **snapshot at open** (a handle reads config once, like a
   git process; a `config`/HEAD change after open needs a reopen); (b) a real **access failure** (`EACCES`,
   symlink loop) *skips* rather than fatals, because the store folds it into the same `Backend` error as the
   `ENOTDIR` that git *does* skip — the store hides the distinction; (c) a **trailing-`/`/`.`** terminal
   (`x/`, `.`) is normalised rather than carrying git's must-be-a-directory requirement; (d) a **symref chain**
   `HEAD → a → b` matches on the first target for `onbranch:`, not the chain's end. These are exotic and bounded
   by the capability model; each is a deliberate limit, not silent breakage. An empty/`.`/`sub/..` include value
   — which resolves to the config directory — **does** abort as a directory target, matching git.

   (Follow-up, landed after the four slices: the component now layers `<git-dir>/config.worktree` under
   `extensions.worktreeConfig`, closing the gap slice 4 documented as out of scope — see the paragraph above.)

   9 host e2e tests (included `packSizeLimit` reaching `repack`; `onbranch` via HEAD; cycle/paradox/directory
   aborting open; `..` through a missing prefix skipped and an in-root `..` resolved; linked-worktree includes
   from the common dir; benign relative include).

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

- **`hasconfig` URL-paradox vs. a later *syntax error / valueless include* in the same forbidden subtree.**
  git's pre-scan reads incrementally and fatals the moment it sees a forbidden `remote.<name>.url`, before any
  later syntax error or valueless `[include] path` in the same file. gitana's `scan_remote_urls` parses each
  included file atomically and *defers* `forbidden_url` (it must, so the driver can combine the trigger across
  layers), so a URL lexically preceding a syntax error surfaces the parse error, and a valueless include later
  in the subtree is simply skipped. Both still fail closed; only the message/precedence differs. Matching git
  exactly would need an incremental parser — disproportionate for this exotic broken-config edge. (P3.)
- **A valueless include reached *only* via the forced-true pre-scan.** `scan_remote_urls` forces `hasconfig`
  true, so it may descend a `hasconfig` include that would not really match; a valueless `path` there is
  skipped rather than fatalling `missing value` as git's forced pre-scan would. The real read reports it on a
  genuinely matched path; the gap is a `hasconfig`-subtree-only, already-broken-config edge.
- **Config-file directory resolution under symlinks — RESOLVED in slice 3 (both halves).** git resolves a
  relative `include.path` against the config file's **lexical** directory (the path it was reached through)
  but a `gitdir:./` condition against its **real** (symlink-resolved) directory. The engine threads both: the
  driver passes the lexical parent and the realpath'd parent of each layer's file, and `IncludeResolver::read`
  returns each included file's canonical path so nested includes get the same split. (The wasm driver, slice 4,
  returns the requested path unchanged — its `FileStore` has no symlink notion, so lexical and real coincide.)
- **`gitdir:` matched against BOTH the realpath and the `$PWD`-honoured spelling — RESOLVED in slice 3.** git's
  `include_by_gitdir` matches a `gitdir:` condition against `realpath(git_dir)` **and**, if that fails,
  `strbuf_add_absolute_path(git_dir)` — the absolute, symlink-*preserving* spelling git forms from the `$PWD`
  it honours. So a repository entered through a symlink at its root matches a condition spelled with the
  symlink path. `IncludeContext` now carries a second candidate `gitdir_absolute`, and `gitdir_matches` tries
  both (git's `again:` loop). The driver derives it (`logical_gitdir`) from `$PWD` — honoured only when it
  canonicalizes to the effective cwd — as `$PWD` joined with the gitdir *relative to that cwd*: `.git` for an
  ordinary root and, for a **bare** root (whose gitdir *is* the cwd), `"."` — reproducing git's unnormalised
  `getcwd + "/."`, so a trailing-slash condition (`gitdir:/link.git/` → `…/link.git/**`) matches the bare root
  as git's does. The spelling survives only at the repo root (probed 2.50.1: not from a subdirectory, where
  git records the realpath after walking up; and never under `-C`/unset/canonical `$PWD`). A final
  `canonicalize` check means a mis-derivation degrades to canonical-only, never a spurious match. *Niche
  remainders (canonical-only, safe):* a linked worktree or a symlinked `.git` — git's abspath spelling for
  those is not `$PWD`-derived; oracle-tested that gta does not over-match. (The wasm component, slice 4, has no
  ambient `$PWD` — and no gitdir path at all — so its `gitdir:` conditions never match, an inherent capability
  limit.)
- **Command-scope (`-c` / `GIT_CONFIG_*`) participates in include processing — handled in slice 3.** git reads
  command-line config as part of the sequence, so a `-c include.path=<abs>` is expanded and a `-c
  remote.<n>.url` feeds a file-level `hasconfig` and the paradox pre-scan; the driver threads the command-scope
  source through `expand_layers` for that, then overlays it (writes still target the local file). The
  command-scope source is **fileless**, which the engine models with dedicated
  `expand_includes_command_scope` / `scan_remote_urls_command_scope` entry points (re-probed vs git 2.50.1):
    - a **relative** `-c include.path` is **fatal** (`ConfigError::IncludeRelativeFromCommandScope`, git's
      "relative config includes must come from files") — *not* silently resolved against the process cwd (an
      earlier draft's "minor deferral" mis-described this as fail-closed; it was fail-**open**, now fixed);
    - a `gitdir:./` condition **does not match** (git prints a non-fatal warning and skips it) — the engine
      returns non-matching rather than rooting the pattern at `/`;
    - `~/`-based and absolute includes expand as in git.
  Command-scope entries are built order-preserving (`GitConfigSource::append`, one block per entry) so an
  `include.path` interleaved with repeats of a key keeps git's linear last-entry-wins order — the grouping
  `add` does would move the include past a later same-section entry.
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
