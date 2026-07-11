# Plan: make core-crate `read_config()` consumers global/system-aware

Follow-up to the merged global-gitconfig work (`6bc1670d`). That pass made **identity + the
`gta config` command** honour git's full precedence stack (system → global → local + `-c`
overlay). Core-crate reads that git also resolves from the merged stack still see **only the
repo-local `.git/config`**, because the engine holds a file-store capability scoped to the git
dir — it cannot reach `~/.gitconfig` or `/etc/gitconfig`. This slice threads the effective
(merged) config into those core reads.

Lead git-faithful.

## The gap (verified on `main` `6bc1670d`)

Keys git reads merged, that gitana currently reads local-only:

- `gitana-porcelain/src/remote.rs:110` — `remote.origin.tagopt` (fetch tag-follow mode)
- `gitana-porcelain/src/remote.rs:117` — `remote.origin.fetch` refspecs (`parse_fetch_refspecs`)
- `gitana-porcelain/src/remote.rs:507` — pull's config read
- `gitana-porcelain/src/remote.rs:1050` — `remote.origin.push` refspecs
- `gitana-repository/src/repository.rs:107` — `pack.packSizeLimit` (`pack_size_limit`)
- `gitana-repository/src/refs.rs:1071` — `core.logallrefupdates` (`reflog_policy`) — **high value**,
  commonly set globally to enable reflogs; also note this one reads the raw file via
  `self.files.read_path("config")`, *not* through `read_config()`

## Decisions (agreed with Scott)

1. **Mechanism: repo-held effective config + a new accessor.** `Repository` carries an
   `Option<GitConfig>` (the effective/merged config), injected CLI-side after open. A **new**
   `effective_config()` method returns it, falling back to the raw-local `read_config()` when
   unset. `read_config()` itself stays **raw-local**.
2. **`core.bare` stays local.** git technically reads it merged, but a *global* `core.bare` is a
   footgun and effectively always repo-local. Left local; deviation noted here.
3. **Process:** this short note, then implement (no `docs/hlds/*.md`).

## Why `read_config()` must stay raw-local (the finding that ruled out overloading it)

The handoff floated overloading `read_config()` to return the merged view. That is **unsafe**:
`gta-core/commands/config.rs:147` uses `read_config()` for the `gta config` **read-modify-write**
path — it reads the local file, mutates, and writes it back. A merged `read_config()` would flatten
global/system entries into `.git/config` on any `--local` write. So the merged view must be a
**separate** accessor.

Repo-format detection is structurally safe regardless: both format reads already bypass
`read_config()` — `dispatch.rs:24` `detect_algorithm` reads `common_dir/config` via
`std::fs::read_to_string`, and `gitana-repository/src/config.rs:85` `Config::read` reads
`store.read_path("config")` directly. Neither goes near the override. A global
`extensions.objectformat` therefore cannot change a repo's format.

## Design

### `gitana-repository`

- `Repository<F, H>` gains a field `effective: Option<GitConfig>` (algorithm-independent).
  - `pub fn set_effective_config(&mut self, config: GitConfig)`
  - `pub async fn effective_config(&self) -> Result<GitConfig, RepositoryError>` — returns the
    override clone if set, else delegates to `read_config()` (raw-local). One clone per call is
    fine (config is small; callers already parse on each read today).
- `pack_size_limit` (`repository.rs:107`): `self.read_config()` → `self.effective_config()`.
- `RefStore<'a, F, H>` gains `effective: Option<&'a GitConfig>` (a borrow, same lifetime as its
  `files: &'a F`). `Repository::refs()` lends `self.effective.as_ref()` into it. `RefStore::new`
  keeps its current signature and defaults `effective: None`; a `with_effective(...)` builder (or
  an added arg on the internal construction in `refs()`) supplies it — so the many
  `RefStore::new(&files)` test sites are untouched.
- `reflog_policy` (`refs.rs:1071`): when `self.effective` is `Some`, read `logallrefupdates` /
  (bare default) from it; when `None`, keep the current raw-local file read + `Enabled` fallback.
  Backward compatible for tests and the wasm component.

### `gitana-porcelain`

- `remote.rs` `repo.read_config()` → `repo.effective_config()` at the tagopt/fetch (:105), pull
  (:507), and push-refspec (:1050) sites. No new params, no capability trait — the merged view
  rides on the `Repository` the caller already passes.

### `gta-core` (injection)

- `dispatch.rs open::<H>()` — the universal open chokepoint — becomes `async`, and after opening
  calls `crate::git_config::effective_config(&repo)` and `repo.set_effective_config(...)` before
  returning. Every dispatch path (`on_repo`/`on_worktree`/`on_object`/`resolve_object`) already
  awaits, so the ripple is mechanical (`open::<H>(found)?` → `open::<H>(found).await?`).
- Runs inside the existing `with_command_cwd` scope, so relative `$GIT_CONFIG_*` overrides resolve
  under `-C` exactly as the identity pass established.

**Blast radius / parity note:** every command now parses the global+system layers and aborts on a
malformed one. That *matches* git, which aborts any command on a bad config file it reads, and
extends the fatal-on-malformed behaviour the identity pass already established for identity-bearing
commands. Accepted as more git-faithful, not less.

### wasm component

- Opens `Repository` through its own path (`component.rs`), never calling `set_effective_config`,
  so `effective_config()` transparently falls back to local-only. Honest: the sandbox has no
  ambient global/system config. No change required beyond it compiling against the new API.

## Migration checklist

- [x] `Repository`: `effective` field + `set_effective_config` + `effective_config()`
- [x] `Repository::pack_size_limit` → `effective_config()`
- [x] `RefStore`: `effective: Option<&GitConfig>` + `refs()` lends it (`with_effective_config`) +
  `reflog_policy` consults it (`logallrefupdates` merged; `core.bare` fallback stays local)
- [x] `gitana-porcelain::remote` three read sites → `effective_config()`
- [x] Injection moved to `gta-core::repo::open_generic` (async) — the true mint chokepoint, since
  `fetch`/`pull`/`push`/`clone`/`trust`/`worktree` open there directly, bypassing `dispatch::open`.
  `dispatch::open` is now a thin async wrapper.
- [x] `core.bare` reads left local: `refs.rs` reflog_policy reads it from the local config, and
  `remote.rs` fetch reads it via `repo.read_config()` (**not** the merged `config`) — a codex-caught
  regression where switching the whole `config` variable to the merged view had leaked a
  global/system `core.bare` into fetch/pull bare-ness.
- [x] repo-format reads left raw-local (detect_algorithm, Config::read) — no change

## Implementation note: injection point

The plan said `dispatch::open`. During implementation I found the remote commands (`fetch`, `pull`,
`push`) and `clone`/`trust`/`worktree` call `repo::open_generic` **directly**, not through
`dispatch::open` — and those are the primary `remote.*` consumers *and* ref-writers. So the
injection went into `open_generic` (the single mint point for ambient FS authority), made `async`,
with all 12 call sites awaited. This also correctly extends merged `logallrefupdates` to clone's and
worktree's reflog writes. A malformed local config still aborts these commands via
`detect_algorithm` (which parses config first, before `open_generic`), so `effective_config`'s
tolerant local read — needed for the fresh init/clone case — introduces no faithfulness regression.

## Implementation note: `core.bare` must be read from local, not the shared merged variable

In `remote.rs` `fetch`, one `config` variable served both `remote.origin.*` (merged) and the
`core.bare` bare-ness check. Switching it wholesale to `effective_config()` inadvertently made
`core.bare` merged too — a global `core.bare=true` would make `pull` refuse fetching into a non-bare
worktree's own branch (and `core.bare=false` could make a bare repo accept a fetch into `HEAD`).
Fixed by reading `core.bare` from `repo.read_config()` (raw-local) while `tagOpt`/refspecs stay on
the merged view. The lesson generalises: when one merged config value feeds several keys, any
local-only key (repo identity: `core.bare`, format) must be read from `read_config()` separately.

## Verification

Oracle harness as in the identity pass (`crates/cli/gta/tests/git_config.rs` helpers: `gta_env`
/ `git_env` / `ISOLATION_ENV`). Point `GIT_CONFIG_GLOBAL` at a temp file carrying, with no local
equivalent:

- `remote.origin.tagopt = --no-tags` → assert `gta fetch` follows git's tag behaviour
- `core.logallrefupdates = true` (and `= false`) → assert reflog write/no-write matches git
- `pack.packSizeLimit = <small>` → assert `gta repack` splits like git

Regression guard: a global `extensions.objectformat = sha256` must NOT change a sha1 repo's format
(stays local — repo-format reads bypass the override).

Full workspace green incl. `wasm32-wasip2`; `cargo fmt`; `codex review --base main` clean before
Claude squash-merges (after Scott approves).
