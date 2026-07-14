# Linked-Worktree Library (`gitana-linked-worktree`)

## Status and Audience

Design pass for a new core crate that exposes Gitana's **linked-worktree management** as an in-process
Rust library. It answers the normative requirements in
`docs/code-henge-linked-worktree-requirements.md` (the consumer, **Code Henge**, manages persistent
editable workspaces). **No code in this document** — it lays out the crate, the identity/object-format
model, the structured-outcome vocabulary, the partial-state classification model, the native-path and
concurrency interpretation, the slice plan, and the validation strategy.

Decisions already taken with Scott (do not relitigate): short HLD first; a **new** core crate
(`gitana-linked-worktree`), not an extension of `gitana-worktree`; extract-and-rewire (the CLI is
pointed at the library in a later slice); **slice 1 is read-only** (inspection + enumeration + the
structured types + the classification enum).

## Context

Code Henge needs to create/inspect/reconcile/remove Git *linked worktrees* **in-process** — no CLI, no
stdout/stderr, no process-CWD changes — receiving **matchable** structured success/observation/
refusal/failure data with preserved error chains, native filesystem paths for identity, SHA-1 **and**
SHA-256, bare + ordinary + linked-worktree discovery contexts, git parity, and consumability at a
pinned Git revision with no sibling path dependency.

Almost all of the *primitives* already exist as reusable, capability-clean core crates. The gap is
that the linked-worktree **admin lifecycle** lives only in the CLI, `anyhow`-typed and printing to
stderr, with no structured surface an external library consumer can use.

## Reuse inventory (what already exists — this crate orchestrates it)

| Need | Existing core piece |
|---|---|
| Discovery (ordinary/bare/linked) | `gitana-repository-layout`: `discover`/`try_discover`/`common_dir_of`/`inspect_root`, `RepositoryLayout { worktree_root, git_dir, common_dir }`, `DiscoveryError` (splits genuine absence from corruption) |
| 3-way status / cleanliness | `gitana-worktree`: `WorkTree::status()` → `Status { changed, untracked }` (staged/unstaged/untracked/conflicted/missing, `.gitignore`, `porcelain_v1()`); `WorktreeError` (thiserror, 27 variants) |
| Refs, CAS, HEAD, reflog | `gitana-repository`: `Repository` (`peel_to_commit`/`rev_parse`/`commit_tree`), `RefStore::transact`/`RefOp` (`RepositoryError::RefMoved`/`RefLocked` races), `HeadState { Symbolic, Detached }`, `detect_hash_kind` |
| Object format | `gitana-object`: `ObjectId<H>`, `HashAlgorithm` (Sha1/Sha256 markers), runtime `HashKind` |
| Per-worktree/common FS routing + capability | `gitana-file-store-local`: `WorktreeFileStore`, `CapWorkDir`/`WorkDirFs`, `LocalFileStore` |
| Structured-outcome precedent | `gitana-porcelain`: `MergeOutcome`/`RebaseOutcome`/`FetchOutcome` with `Conflict` variants inside `Ok(...)` (this crate mirrors the *shape* but with `thiserror`, not `anyhow`) |

Core crates already have **zero stdout/stderr** and **no process-CWD reads** (ambient FS is an
injected `WorkDirFs`/`cap-std` capability), and use RPITIT / plain `async fn` (no `async-trait`), with
`tokio` narrow and wasm-gated.

The admin lifecycle to be lifted lives in `crates/cli/gta-core/src/commands/worktree.rs` (1601 lines)
and `crates/cli/gta-core/src/repo.rs` — private, `anyhow`-typed, `eprintln!`-ing, `cwd`-threading
functions (`enumerate_worktrees`, `info_for`, `read_head`, `read_lock_reason`, cross-pointer helpers,
`branch_checkout_location`, `is_bare`, `worktree_path_of`, `is_dirty`, …).

## The crate

`crates/core/gitana-linked-worktree/` — a normal `crates/core/*` workspace member (picked up by the
existing glob; no other manifest change). Deps: `gitana-repository-layout`, `gitana-repository`,
`gitana-object`, `gitana-worktree`, `gitana-file-store`, `gitana-file-store-local`, `gitana-config`,
`gitana-object-store`, `thiserror`, `tokio` (rt); `cap-std` under `cfg(not(target_arch = "wasm32"))`.

**Consumability:** every intra-repo dep is a workspace `path` dep; when Code Henge depends on
`gitana-linked-worktree` as a `git = …, rev = …` dependency, Cargo resolves those path deps inside the
pinned checkout. The "no sibling path dependency" requirement is about the *consumer* not vendoring
gitana — nothing here blocks a git-rev dependency.

### Identity model — explicit, anchored on the common dir

The doc requires the caller to "identify the repository explicitly enough that Gitana does not infer
project ownership from the destination path." The identity anchor is the shared `.git`
(**`common_dir`**) — linked worktrees of one repo share it; a destination path never identifies a
repo. The destination is a *query argument*, not identity.

```rust
pub struct RepositoryId { common_dir: PathBuf, git_dir: PathBuf, worktree_root: Option<PathBuf> }
impl RepositoryId {
    async fn discover(start: &Path) -> Result<Self, LinkedWorktreeError>; // ordinary/bare/linked contexts
    fn at_common_dir(common_dir: PathBuf) -> Self;                        // fully explicit, no walk, no CWD
}
```

### Object-format stance — non-generic public API, internal dispatch

The public API is **non-generic**; internal bodies read `extensions.objectformat` (via
`detect_hash_kind` / the existing `detect_algorithm` logic) and dispatch to `..._generic::<Sha1>` /
`::<Sha256>`. Object ids cross the boundary as a runtime-tagged type, so Code Henge is format-agnostic
and never monomorphizes:

```rust
pub enum WorktreeObjectId { Sha1(ObjectId<Sha1>), Sha256(ObjectId<Sha256>) }
// kind() -> HashKind, to_hex() -> String, parse(kind, hex) -> Result<Self, _>
```

### The FS mint moves in; the global-config merge does not

A crate-internal `open_store::<H>(git_dir, common_dir)` mints `cap-std` authority from **absolute**
paths (`Dir::open_ambient_dir`, never changes CWD) and builds
`Repository::new(ObjectStore::new(WorktreeFileStore::new(common, git)))`. It deliberately does **not**
fold in the CLI's `git_config::effective_config` (global/system config from `$HOME` — a
user-environment concern). Slice 1 reads only `extensions.objectformat` + `core.bare` from
`<common>/config` (no global layer needed). The **create** slice will accept an *optional injectable
effective-config* (default local-only) for `core.logAllRefUpdates` reflog parity, so Code Henge
decides whether the user's global config applies — matching "permit Code Henge to supply its own …
policy." The crate is native-gated for the mint (Code Henge is a native consumer; wasm can inject
capabilities later).

### Outcome vocabulary — refusals are `Ok`-data, failures are errors

The doc distinguishes structured **observation/refusal** (a fact about the world) from **failure** (an
operation could not be carried out). This crate encodes that split:

- **`WorktreeClassification`** (an `Ok` value) — the partial-state model (below).
- **`LinkedWorktreeError`** (`thiserror`) — hard failures only: `Discovery(#[from] DiscoveryError)`,
  `Io { context, path, #[source] }`, `MalformedPointer { kind, path }`,
  `Repository(#[from] RepositoryError)`, `Worktree(#[from] WorktreeError)`, `UnsupportedObjectFormat`.
  `#[from]`/`#[source]` preserve chains. **"A status failure MUST NOT be treated as clean"** ⇒ a
  `Worktree(_)` error, never a `Complete` classification.

### `WorktreeInspection` — every field the Inspection Requirements enumerate

A read-only snapshot of one destination against one `RepositoryId`, capturing: `destination` +
`expected_branch` (echoed identity, so a result is never misapplied to a replaced path);
`destination_kind` (`Absent | EmptyDir | LinkedWorktreeCheckout | OtherFsObject | UnrelatedContent`);
`registration` (`None | Present{admin_dir} | PresentCheckoutMissing{admin_dir}`); `cross_pointers`
(`NotApplicable | Consistent | Inconsistent{checkout_points_to, admin_points_to}`); `git_dir` +
`common_dir`; `head: Option<HeadFacts { state: Symbolic|Detached|Unborn, branch, object }>`;
`requested_branch` (`NotRequested | Absent | Exists{ object, checked_out_elsewhere }`); `lock`
(`Unlocked | Locked{reason}`); and `identity_conflict: Option<IdentityConflict>`. `inspect()` never
prunes, repairs, or rewrites observed state.

### Partial-state classification — pure, total, precedence-ordered

`classify(&WorktreeInspection) -> WorktreeClassification` is a **pure function over an already-built
inspection** (no I/O). The requested `start` and its ancestry relation to the worktree's current object
both live on the inspection (`inspect` computes the reachability), so a fast-forward is told apart from a
rewind/divergence here without any walk. It maps the doc's Partial-State table:

```
ProtectedWithReason   (Locked now; Dirty folds in with the removal slice)
DestinationConflict   (OtherFsObject | UnrelatedContent)
IdentityConflict      (cross-pointer disagreement outranks canonical-path equality)
BranchUseConflict     (requested branch checked out at another destination)
PartialConflicting    (checkout present, registration missing/inconsistent)
PartialRegistered     (registration retained, checkout gone)   ← distinct from "fully absent"
CompleteIdempotent / MatchingAdvanced  (exact match; advanced branch is reported, never reset)
CompletePresent       (registered + consistent, but detached/unborn — healthy, not an exact match)
InterruptedCompletable (branch created at start, worktree never finished)
AbsentSafeToCreate
```

Precedence is most-specific-refusal-first; the function is total over any inspection.

### Enumeration

`enumerate(&RepositoryId) -> WorktreeListing { entries: Vec<WorktreeEntry> }`, primary first then
linked (admin-name sorted). Each `WorktreeEntry { role: Primary{bare} | Linked{admin_dir}, path,
head: HeadKind, branch, object, checkout_missing, lock }` — covering primary + linked +
missing-registered + detached + bare + lock + branch + current object.

### Status readout

`WorktreeStatusReport` wraps `gitana-worktree`'s `Status` (clean/staged/unstaged/untracked/conflicted/
missing + `porcelain_v1()`), tied to the inspected identity so a stale result is never applied to a
replaced path. A status computation *failure* is a `LinkedWorktreeError`, never "clean".

### Native paths

Identity/destination/cross-pointer paths are `PathBuf`/`&Path` end-to-end, compared by canonicalized
`Path` equality. The CLI's lossy `to_string_lossy` boundaries (unique-suffix worktree resolution,
human rendering) are **not** lifted — they are CLI/DWIM conveniences; the library resolves by exact
canonical path. The pre-existing UTF-8 constraint on *tracked working-tree filenames* (`FileStore`/
`WorkDirFs` are `&str`) is unrelated (it constrains file contents, not identity paths) and out of
scope.

### Concurrency interpretation

The doc requires "a lost race MUST be reported as a conflict rather than overwriting the winner," but
its Non-Requirements explicitly waive crash-atomicity ("not required to guarantee a multi-effect
operation is atomically crash-free"), requiring instead that partial effects be *observable and
classifiable on retry*. So: the **branch ref** goes through the existing `RefStore::transact`/`RefOp`
CAS (a concurrent branch move surfaces as `RefMoved`); **admin-file** effects match git (git is not
atomic here either) and are made safe by (a) re-verifying identity immediately before any destructive
effect and (b) every partial state being classifiable by `inspect`/`classify`. Slice 1 is read-only,
so this is realized in the create/remove slices; full registration-level CAS + native-`OsStr` polish
is the final hardening slice.

## Slice plan

1. **Read-only inspection + enumeration + types + classification** (this HLD's slice 1) — pure reads,
   zero durability risk; defines the vocabulary every later slice classifies against.
2. **Create / reconcile** — explicit-only creation, idempotent completion, CAS branch creation,
   admin-layout write, checkout materialization, injectable effective-config for reflog parity.
3. **Remove + status-surface finalization** — identity-rechecked safe removal (refuse dirty/
   conflicted/locked/primary/mismatch; preserve branch + untracked), `Dirty` folded into classification.
4. **CLI rewire** — point `commands/worktree.rs` + `repo.rs` at the library; keep DWIM/force/
   `--porcelain`/suffix resolution on top. Behavior-preserving; existing git-parity tests stay green.
5. **Concurrency + native-path hardening** — registration-level CAS, lost-race-as-conflict, full
   `OsStr` identity paths, lock-file races.

### Deferred (slice-5 hardening)

- **Discover-time git-faithful gitfile parsing.** `RepositoryId::discover` delegates `.git`-pointer
  parsing to `gitana-repository-layout`, which takes the first line and trims — so a repository whose git
  dir path ends in a space or contains a newline (both git-legal, and handled by `at_common_dir` +
  inspection here) fails `discover`. Aligning the layout crate's parser with the full-body,
  whitespace-preserving rules used in `pointers::gitfile_target` is a cross-crate follow-up.
- **Per-worktree pseudoref object resolution.** A HEAD chain terminating in a one-level pseudoref that
  *directly* holds an object id (`HEAD -> refs/heads/alias -> CUSTOM_REF`, `CUSTOM_REF` an admin-local
  OID) resolves the terminal *name* correctly, but the object read routes through
  `WorktreeFileStore`, which does not route arbitrary one-level pseudorefs to the admin dir — so the
  object may read from `<common>` and report unborn/wrong. The fix is per-worktree routing in the store
  crate (`gitana-file-store-local`).
- **git's full boolean grammar for `core.bare`.** `GitConfig::get_bool` (shared crate) accepts the
  `true/false/yes/no/on/off/1/0` forms but not git's numeric booleans (`2`, `-1`, `1k` — any nonzero is
  true). A repo with `core.bare = 2` errors instead of being seen as bare. Expanding the shared boolean
  parser to git's integer grammar is a config-parser follow-up.
- **Retained registration when a checkout is *replaced by a symlink*.** `admin_dirs_for` refuses a
  symlinked destination outright (to avoid following an alias to a live checkout). That also drops a
  *retained* registration whose own recorded path was deleted and replaced by a symlink — git still lists
  it (prunable) and honors its lock. Distinguishing "the recorded path itself is now a symlink" (match,
  unfollowed) from "a different symlink resolving to a registered checkout" (no match) needs a
  leaf-not-following, case-insensitivity-aware path comparison; deferred as a slice-5 refinement. Impact
  is a less-precise classification (`DestinationConflict` vs `PartialRegistered`) for that exotic state —
  both block a create/reconcile, so no safety hole.
- **`core.bare` under `extensions.worktreeConfig`.** When `extensions.worktreeConfig=true`, git reads the
  primary/bare context's `core.bare` from `<common>/config.worktree`; `is_bare` reads only `<common>/
  config`. A bare repo that moved `core.bare` into `config.worktree` would be seen as non-bare. Merging the
  enabled worktree-config layer is a config-layering follow-up (the same layered `GitConfig` the CLI edge
  already uses).
- **Windows case-insensitive missing-path matching.** The `canonical_eq` fallback that folds ASCII case
  for a *deleted* checkout queried by a case-variant path is `cfg(unix)` (inode-probe based). On Windows
  (also case-insensitive by default, but without inodes) it falls back to a case-sensitive string
  compare, so a case-variant query of a missing registration can misclassify. A Windows-native
  case-fold probe is a follow-up; the crate's tests are `cfg(unix)` and the current consumer is native
  Unix/macOS.

## Validation

Read-only oracle tests vs stock `git` (the established pattern: build fixtures with
`std::process::Command`→`git`, run the read fns, assert; guard SHA-256 with `git_supports_sha256()`),
**every case parametrized over SHA-1 and SHA-256**: AbsentSafeToCreate; InterruptedCompletable;
CompleteIdempotent (+ re-inspect stable); PartialRegistered (cross-checked against `git worktree list
--porcelain` `prunable`); inconsistent cross-pointers (→ IdentityConflict/PartialConflicting, **no
silent repair**); BranchUseConflict (oracle: `git worktree add` refuses); DestinationConflict for
file / non-empty dir / **symlink** (a destination that *is* a symlink is `OtherFsObject`; a directory
whose inner `.git` is a symlink is `UnrelatedContent` — the symlink is **never followed**);
MatchingAdvanced (commit after add → current object, not reset); status readout compared to
`git status --porcelain=v1` (a status *failure* → `Err`, never `Complete`); bare context; linked
discovery context (common_dir resolves to the shared `.git`); enumeration parity vs
`git worktree list --porcelain`. Create/remove/concurrency cases are deferred to their slices. The
workspace's `cargo fmt`/`clippy`/tests/wasm-target checks stay green; `codex review --base main` loops
until clean before merge.

## Mapping to the requirements doc

- *Required Scope, Inspection Requirements, Partial-State table, Status/Cleanliness* → slice 1
  (`inspect`/`classify`/`enumerate`/`status` + the types above).
- *Explicit Creation Inputs, Creation/Reconciliation* → slice 2.
- *Removal Requirements, "no force mode for Code Henge"* → slice 3 (the safe removal is the library
  default; force stays a CLI-only concept).
- *Library-Consumer Requirements* (structured data, error chains, no stdout/stderr, no CWD, git-rev,
  no path dep) → satisfied crate-wide by construction (see "The crate").
- *Data-Preservation & Security* (never follow a `.git` symlink, never replace a non-directory, cross-
  pointer identity outranks canonical-path equality, native paths) → the `DestinationKind`/
  `CrossPointerHealth` model + native-path decision.
- *Git Compatibility* → oracle-tested vs stock git across both object formats and all discovery
  contexts.
