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
3. **Remove + status-surface finalization** — ✅ **DONE.** Identity-rechecked safe removal
   (`remove(&RemoveRequest) -> Result<RemoveOutcome, RemoveError>`): inspect-first with a status run, decide,
   then re-inspect + re-decide **immediately before any destructive effect** so a lost race is a conflict, not
   an overwrite. Refuses dirty/conflicted (`ProtectedWithReason::Dirty`, carrying the report), locked, primary,
   and identity-mismatch (`expected_branch` pin → `RegisteredToDifferentBranch`); retains the branch + commits
   (never deletes a ref); preserves unrelated content; idempotent once absent; **no force mode**. `Dirty` folds
   into `classify` via an opt-in `WorktreeQuery.with_status` + `WorktreeInspection.status` (a status *failure*
   is a hard error, never "clean"; a plain inspect stays status-free). Absorbs the deferred **recoverable
   mid-checkout** item at the *classification* level: `classify`/`create::decide` read an owned
   `PresentCheckoutMissing` **whose destination is absent or empty** as `PartialRegistered` (not a
   `DestinationConflict`), so an empty/absent partial is prune-and-retryable. `RemoveOutcome` (`Removed` /
   `AlreadyAbsent`) and `RemoveError` (`Refused(WorktreeClassification)` / `IsPrimaryWorktree` / `Incomplete` /
   `Failed`). **Two removal-safety decisions taken with Scott (after three codex rounds all circling the same
   `remove_dir_all`-vs-preservation tension):**
   - **Empty-only partial cleanup (git-parity).** A recoverable partial is cleaned (admin dropped, empty
     leftover removed with `remove_dir`, not `remove_dir_all`) **only when the destination is absent or an
     empty directory**. A *non-empty* directory at a checkout-missing registration is refused and preserved
     (a `DestinationConflict`) — no historical signal (registration, or an in-admin marker) proves the current
     directory is this worktree's own vs a reused path or a git-created prunable, so deleting it is unsafe;
     this matches git's own `worktree remove`, which refuses a prunable-with-directory. (An in-admin
     checkout-in-progress marker was prototyped and **rejected**: it proves only a historical create attempt,
     not current-directory ownership, so codex defeated it with a delete-and-reuse race.)
   - **Conservative preserve-mode for residual content (reverses an earlier git-parity call).** A live checkout
     is removed only when its working tree contains **solely tracked files** — a *matcher-independent*
     index-membership scan (`residual_untracked_paths`), separate from `is_clean()`. Any residual untracked *or
     ignored* content is refused (a distinct `ProtectionReason::ResidualContent { paths }`, a capped sample so
     the caller knows what to clear) and preserved. This deliberately diverges from `git worktree remove` (which
     deletes ignored build artifacts): `gitana-worktree`'s `.gitignore` matcher is not fully git-faithful
     (codex round 5 found `foo\*`, `***/b`, and it does not handle `\`-escapes / `info/exclude` / global
     excludes), so trusting it for a recursive delete could destroy a git-*untracked* file. The scan needs no
     matcher, so no ignore bug can ever authorise deleting a non-tracked file. Membership is **exact** — case
     is deliberately *not* folded (config `core.ignorecase` can be set on a case-sensitive volume, and
     per-directory ext4/F2FS case folding defeats any whole-tree probe, so a fold could let a case-distinct
     untracked `FOO` pass as tracked `foo` and be deleted; exact matching is fully safe, at the cost of
     over-refusing a rare mv-based case-only rename) — and (on Windows only) normalises `\` separators to `/`
     (on Unix `\` is a valid filename byte, preserved to avoid an `a\b`/`a/b` collision). The checkout's own
     root `.git` gitfile is excluded from the scan by matching its **real stored name** —
     `canonicalize(destination/.git)`'s leaf (falling back to the literal `.git`) — byte-exact. This skips a
     case-insensitive filesystem's `.GIT`-spelled pointer (canonicalize returns its real `.GIT` name) but never
     an untracked **hard link** to the gitfile under a *different* name (`ln .git backup`, or a case-sensitive
     filesystem's distinct `.GIT` — whose name differs from the gitfile's canonical `.git`): those share the
     inode but keep their own name, so they stay residual and are preserved (removal trusts this scan over
     `status`'s untracked list). Matching the *name*, not the inode, is what distinguishes a case alias from a
     second hard-link entry. `classify` applies
     the *same* tracked-changes/residual checks, so `classify(inspect(...))` agrees with the removal outcome.
     The dirtiness gate reads *tracked-side* changes only (`has_tracked_changes`), **not** `is_clean()`
     — the untracked list from `gitana-worktree::status` can false-positive under `core.ignorecase`/a bad
     ignore match, and the residual scan is the authoritative (case-aware, matcher-free) untracked check — so a
     status quirk never makes a clean worktree un-removable, and untracked vs tracked-modified refusals stay
     distinct (`ResidualContent` vs `Dirty`). (An earlier round chose git-parity "delete ignored"; round 5's
     evidence that the matcher has multiple false-positives reversed it to preserve-mode — the requirements'
     literal "preserve untracked and unknown files".)
   - **Primary before registration.** Primary identity is judged from the checkout itself
     (`main_checkout_identifies_common`), *ahead of and independent of* registration, so a stale/forged admin
     that registers the primary's path can never drive its deletion.
   - **Enclosing-common-dir guard.** A checkout whose destination *encloses* the repository's own common dir
     (a supported relocated-bare/`--separate-git-dir` topology — `<dest>/meta.git`, git-ignored so the tree is
     clean) is refused (`RemoveError::EnclosesRepository`) before any deletion; recursively deleting it would
     destroy the repo's refs/objects, including the retained branch.
   - **Enclosing-common-dir + unborn-branch + case-safe containment.** A checkout that *encloses* the
     repository's common dir (relocated-bare `<dest>/meta.git`) is refused (`RemoveError::EnclosesRepository`)
     ahead of any content check — recursively deleting it would destroy the repo. Containment is judged by
     filesystem identity (`canonical_eq` up the ancestor chain), so a case-insensitive path alias is caught
     too. `RemoveOutcome::Removed`'s `retained_branch` is gated on `HeadKind::Symbolic`, so an unborn orphan
     HEAD retains `None`. The branch is read from the *accepted recheck*, not the stale first inspection.
   - **Companion fix — git-faithful `**` ignore matching (`gitana-worktree`).** The `.gitignore` matcher
     treated *any* `**` as the path-spanning glob, so `a/**b` wrongly ignored `a/a/b` (git reports it
     untracked). `glob_match` now treats `**` as recursive only as a whole path component (`**/`, `/**`,
     `/**/`), matching git; this also corrects `gta status`/`add`/`worktree remove`. (The broader class of
     matcher divergences is why the removal gate does not *trust* the matcher — see preserve-mode above.) The
     gitlink false-*dirty* gap (a clean submodule worktree reads as dirty → over-refuses; safe) remains
     deferred in `gitana-worktree` (see "Still deferred").
   - **Ordered deletion + atomic de-registration.** The admin is dropped **only after the checkout is confirmed
     gone** — a failed checkout deletion keeps the registration (a repairable worktree), never an orphaned
     checkout whose `.git` points at a deleted admin. The admin itself is de-registered **atomically**: it is
     `rename`d out of `worktrees/` (one step, regardless of its children) *before* its bytes are deleted
     best-effort. So an *undeletable* child — an immutable/`chflags`-locked file no tool (git included) can
     unlink — can never leave a *recognisable* half-deleted registration: the moment the rename lands the
     registration is gone, and any lingering bytes are harmless cruft outside `worktrees/`; only a failure to
     even rename (an unwritable `worktrees/`) leaves it in place as a re-inspectable `Incomplete` a retry
     recognises. (Deleting the identity files in place cannot give this: recognition needs both `gitdir` and
     `commondir`, so any deletion order risks unlinking one and then failing on the other.) The trash name is
     `<common>/.gitana-removing.<pid>.<seq>` (a process-monotonic `seq`); if that name already exists — a crashed
     prior run's remnant, reachable after PID reuse restarts `seq` at 0 — de-registration **advances to the next
     sequence** (checked *before* the rename, since POSIX `rename` silently *replaces* an empty target dir, and a
     no-replace `renameat2` would need `unsafe`) rather than failing the rename onto — or clobbering — a remnant
     (which, with the checkout already deleted, would strand the registration as a false `Incomplete`).
   - **`retained_branch` only for shared `refs/heads/*`.** A per-worktree HEAD target (`refs/worktree/*`,
     `refs/bisect/*`, `refs/rewritten/*`) lives inside the admin dir being removed, so it is not reported as
     retained (nor is an unborn orphan HEAD).
   - **Lock-first, even under corrupted administration.** The lock is read **directly** (no HEAD/index parse)
     before any inspection, so a locked worktree with a malformed `HEAD` *or* index still returns the
     structured `Locked` refusal, not `Failed` (git's lock-first order). A no-status `decide_remove` preflight
     then handles primary/enclosure/identity refusals without reading the working tree; only if it would
     proceed to a destructive action is the status-bearing inspection run.
   30 oracle tests (SHA-1 + SHA-256), incl. regressions for the stale-registration-primary, non-empty-partial
   preservation, ignored/residual-content preservation (with reported paths + `classify` agreement),
   branch-use-over-partial, enclosing-common-dir, `**`-false-clean, unborn-orphan, partial-admin-cleanup
   (`Incomplete`), and locked-with-broken-index/HEAD (lock-first) cases, plus `gitana-worktree` matcher and
   path-normalisation unit tests.

   **Deferred to slice 5 (per the concurrency plan below), not a slice-3 regression:** a residual registration
   **TOCTOU** — between the mandatory pre-destroy re-inspect and the `remove_dir_all`, a concurrent
   `git worktree repair`/re-registration is not held off by a lock, so the delete could still overwrite a
   racing winner. Slice 3's promise is the immediate-pre-destroy re-verify (which catches every race up to that
   check) plus classifiability-on-retry; git's own `worktree remove` shares this residual non-atomicity.
   Closing the window fully needs the **registration-level lock/CAS** that slice 5 ("Concurrency + native-path
   hardening — registration-level CAS, lost-race-as-conflict, lock-file races") is scoped to add.
4. **CLI rewire** — point `commands/worktree.rs` + `repo.rs` at the library; keep DWIM/force/
   `--porcelain`/suffix resolution on top. Behavior-preserving; existing git-parity tests stay green.
5. **Concurrency + native-path hardening** — registration-level CAS, lost-race-as-conflict, full
   `OsStr` identity paths, lock-file races. *(First installment landed: the round-4 create-hardening
   tranche — F1 post-condition, F3 atomic pointers, F4 name sanitization. Remaining items — registration
   CAS on create/remove, lock-file races, the broader `OsStr` boundary sweep — follow the remove slice.)*

### Slice-5 create hardening (DONE) and the one item deferred to slice 3

The slice-2 create codex review (round 4) raised four durability / native-path findings (all verified vs
stock git). **Three landed in slice 5** — git-faithful name sanitization, post-condition validation, and
pointer-publication hardening:

- **Post-condition validation of the final inspection (`create`).** ✅ Slice 5. `create` re-runs `decide` on
  the post-write inspection and returns success only when it re-decides to `AlreadyThere`; **any** other
  outcome — `decide` wanting another write, or refusing (a concurrent branch divergence / re-appeared
  conflict / removed destination) — is a single `CreateError::NotEstablished` lost-race error carrying the
  observed state, never a preflight-style refusal or a false `Ok`.
- **Pointer-publication hardening (`create`).** ✅ Slice 5. The admin `commondir`/`gitdir`/`HEAD`/
  `ORIG_HEAD`/`logs/HEAD` are written to a temp sibling, `fsync`ed, then `rename`d into place (atomic and
  universal, so a reader never sees a torn admin pointer and a crash leaves an absent — classifiable — file,
  not a half-written one). The checkout `.git` gitfile is written with a plain exclusive `create_new`
  (`O_CREAT | O_EXCL`) **exactly as git does** — no-clobber (a raced-in `.git`, even a symlink, is refused,
  never followed/truncated) and portable to filesystems without hard-link support (FAT/exFAT, some SMB/NFS)
  where a link/rename publish would fail though git succeeds. This matches git's own non-atomic gitfile
  write; the residual "an interrupted create leaves an empty `.git`" window is git-parity, and the
  gitfile-last ordering keeps it a `PartialRegistered`-class partial (the *recoverable-mid-checkout* item
  below covers making such partials cleanly retryable).
- **git-faithful worktree-name sanitization (`unique_admin_dir`).** ✅ Slice 5. The admin name is sanitized
  from the destination basename's bytes exactly as git does (probed vs git 2.50.1): refname-invalid bytes
  (control, space, DEL, `* : ? [ \ ^ ~`) → `-`, a leading `.` neutralised, a `..` run collapsed, an `@{`
  sequence broken, a bare `@` → `-`, a trailing `.lock` stripped; valid multi-byte (≥ 0x80) sequences pass
  through — while the admin `gitdir` still records the real (unsanitized) destination path. So the admin path
  can never carry the newline/CR that would break the gitfile cross-pointers. **Non-UTF-8 handling is not yet
  complete:** because the cross-pointers still serialize/parse via `Path::display()` / `read_to_string`, a
  non-UTF-8 byte anywhere in the *resolved* destination or common-dir path is **rejected up front** (on the
  write path, so an idempotent no-op is never falsely refused) rather than written lossily. Byte-clean
  pointer I/O — the "full `OsStr` at every boundary" requirement — remains **deferred**.

**Resolved in slice 3 (was deferred from slice 2):**

- **Recoverable mid-checkout state — resolved at the classification level; file-cleanup is empty-only.**
  ✅ (slice 3). git writes the checkout `.git` gitfile **first** (probed), so a mid-checkout failure leaves a
  *registered, dirty* worktree that `git worktree remove --force` recovers. gitana writes the gitfile **last**,
  so an interrupted-*before*-checkout create is cleanly `PartialRegistered`. Slice 3 makes an **absent-or-empty**
  such partial classify as `PartialRegistered` (a prune-and-retry state) in both `classify` and `create::decide`,
  and `remove` cleans it (drops the admin, removes the empty leftover with `remove_dir`) so a retry proceeds.
  **What it deliberately does *not* do:** auto-delete a *non-empty* leftover directory. Three codex rounds
  established that no after-the-fact signal — a registration back-pointer, or an in-admin
  checkout-in-progress marker (prototyped and rejected) — proves the *current* directory contents are this
  create's own rather than a reused path or a git-created prunable; a status-based "clean" check is also
  insufficient (a coincidental clone at the same commit, or ignored content, passes it). So a non-empty
  checkout-missing partial is **refused and preserved** (a `DestinationConflict`), matching git's own
  `worktree remove`. An interrupted-*mid*-checkout that left files therefore still needs the caller (or a
  future force path) to clear them before retry — the narrow, mostly-deferred (submodule-gitlink) tail below.

**Still deferred:**

- **Reachability-model completeness for exotic anchors (codex round 32, deferred after a decision to stop the
  loop).** The commit-preservation reachability check (`first_unreachable_admin_anchor` + `reachable_from_shared_refs`)
  covers `HEAD`, `refs/{heads,tags,remotes,worktree,bisect,rewritten}/*`, and the objects their symbolic forms
  resolve to — but **not** top-level *pseudorefs* as commit anchors: (a) an **admin-local** pseudoref
  (`ORIG_HEAD`, a custom `CUSTOM-REF`) as the sole anchor of an otherwise-unreachable commit is not scanned, so
  removal orphans it (this largely matches git's own `worktree remove`, which drops `ORIG_HEAD`/`logs/HEAD` too);
  (b) a **shared** top-level ref (`<common>/CUSTOM1`) that *survives* removal is not counted as a reachability
  root, so a commit anchored only by it is a *safe over-refusal*. Also: a per-worktree ref tip is passed to
  `is_ancestor` **unpeeled**, so a tip that is a tree/blob (over-refuses) or an annotated tag (reported by tag
  oid) is mishandled — a `try_peel_to_commit`-and-skip-non-commits pass is the fix. These form a completeness
  tail (each fix has surfaced the next more-exotic variant over rounds 24–32); deferred as a bounded follow-up
  rather than continue the loop, since the trigger states are crafted/vanishingly rare and (a) is close to git
  parity.
- **Atomic no-clobber trash rename (codex round 32).** `deregister_admin` renames the admin to
  `.gitana-removing.<pid>.<seq>` after a *pre-rename* existence check, but POSIX `rename` replaces an empty
  target, so a target created in the check→rename window is clobbered. A truly atomic no-replace rename needs
  `renameat2(RENAME_NOREPLACE)` / `renamex_np(RENAME_EXCL)` via `unsafe` libc, which the **workspace forbids**;
  the residual race is in gitana's own private `<pid>`-scoped namespace (unreachable in practice — `seq` is
  process-atomic, so same-process concurrent removals never collide), so it is accepted until a safe no-replace
  primitive is available.
- **`gitana-worktree` status fidelity for exotic git configs/modes (removal's dirty gate).** Removal's
  tracked-changes gate is only as git-faithful as `gitana-worktree::status`. Three config/mode behaviours that
  used to make a *git-clean* worktree read as `Dirty` (a safe over-refusal) are now **implemented as companion
  fixes on this branch** (each oracle-tested vs stock git and also benefiting `gta status`/`add`):
  **`core.fileMode=false`** (an exec-bit-only difference is no longer a modification), **sparse-checkout**
  (`CE_SKIP_WORKTREE` is parsed, preserved on write, and the omitted paths are not compared), and
  **`core.splitIndex`/`git update-index --split-index`** (`load_index` loads the referenced `sharedindex.*` and
  merges it — the merged index matches `git ls-files --stage`).
  - **A removal-only content re-verification hashes every present tracked file (not a `Dirty`/residual gap — a
    data-loss one).** `status()` — like git — takes a stat-cache fast path (`stat_matches` → clean without
    re-hashing) and omits skip-worktree entries entirely, so it can miss a genuine edit: a same-size /
    stat-preserving rewrite, an edit within a coarse-timestamp filesystem's (FAT/exFAT) granularity, or a
    `--skip-worktree`d-then-edited file. Recursive removal must not delete a checkout on that strength, so
    `WorkTree::diverged_tracked_content_paths` re-verifies **by hashing** every present stage-0 tracked file
    against the index (oid + mode under the resolved `core.fileMode`); any divergence refuses removal via
    `ProtectionReason::ModifiedTrackedContent`. An absent entry (a deletion `status` catches, or an omitted
    sparse path) stays removable. A present entry that hashes equal is only "reconstructable" — and so
    removable — if the object store holds a **valid** copy: existence is not enough (a present-but-corrupt loose
    object, or a pack naming an unreadable object), so it **reads the stored blob (`Repository::read_blob`) and
    re-hashes it** against the indexed oid — a read failure or hash mismatch flags the file (the working copy is
    then the sole valid one). This is a removal-only check — `status()` stays git-faithful (stat-cache fast path
    intact).
  - **Every commit anchored only inside the admin dir is preserved (commit-preservation contract).** Removing a
    worktree deletes its admin dir and *every* reference living there: a **detached** (or per-worktree-symbolic)
    `HEAD`, **and** the whole per-worktree ref namespaces `refs/worktree/*`, `refs/bisect/*`, `refs/rewritten/*`
    (git's `is_per_worktree_ref` set). Each such tip uniquely anchors its commit; one reachable from no
    *surviving* shared ref (`refs/heads`, `refs/tags` peeled, `refs/remotes`, …) would be orphaned (later
    gc-able). `status::first_unreachable_admin_anchor` collects those anchor commits (the `HEAD` commit is
    included **only** when its own anchor won't survive — a `refs/heads/*` HEAD is skipped, its branch survives —
    while the per-worktree tips are always checked) and returns the first that is unreachable. Reachability walks
    the shared refs **over the common store specifically** (so this worktree's own per-worktree refs, physically
    in its admin dir, are never miscounted as a surviving anchor) and tests `Repository::is_ancestor`; the tips
    themselves are read from the admin via the routing store — both **direct** tips (`list`) and the objects
    **symbolic** per-worktree refs resolve to (`symbolic_ref_targets` per prefix; `list` skips symbolic refs, so
    `refs/worktree/save -> ORIG_HEAD` anchoring a commit would otherwise be missed). Reachability's roots are
    likewise both the **direct** shared refs (`RefStore::list("refs/")`) **and**
    the objects **symbolic** shared refs resolve to (`symbolic_ref_targets("refs/")` — a commit anchored only via
    a symbolic tag/branch such as `refs/tags/anchor -> CUSTOM1` would otherwise be
    reported unreachable and the clean removal spuriously refused). An unreachable anchor refuses via
    `ProtectionReason::UnreachableAnchoredCommit { commit }` so the caller can branch/tag it first, then retry. A
    HEAD symbolic to a surviving `refs/heads/*` branch is not passed as an anchor (its branch survives). The check
    runs for a **live checkout and a checkout-missing partial** alike (both delete the admin), and `classify` /
    `decide_remove` share one `unreachable_head_protection` helper so they agree. Reachability from *another*
    worktree's detached HEAD is deliberately not consulted — omitting it can only *over*-refuse (the safe
    direction), never delete an orphan.
    - **A checkout-missing partial with staged/unmerged index work is refused.** A `PresentCheckoutMissing`
      partial still holds its per-worktree index; cleaning it drops that index, erasing staged state and
      orphaning index-only blobs. `status::partial_has_staged_changes` compares the retained index to `HEAD`
      **without a working tree** (the checkout is gone) — any unmerged entry, or an index/tree difference,
      refuses via `ProtectionReason::StagedContentInMissingCheckout` (a live checkout's staged changes are
      already a `Dirty` refusal via `status`). `classify` and `decide_remove` share a `partial_staged_protection`
      helper. An **absent** index (no `index` file — `create` interrupted after `HEAD` was published but before
      the checkout materialised it) is *not* staged work: `has_staged_changes` returns `false` for it (probing
      the file's existence), so such a recoverable partial cleans rather than refusing forever (it must not be
      conflated with an empty index vs a non-empty `HEAD`, a spurious all-paths staged deletion).
    - **A trailing-separator symlink destination cannot delete its target.** A destination spelled `.../wt-link/`
      makes `symlink_metadata` *follow* the leaf symlink (POSIX), so a naive stat would misclassify it as the
      target directory and a canonical delete would destroy the real worktree, leaving a dangling link.
      `classify_destination` (and `is_leaf_symlink`) stat the **trailing-separator-stripped leaf** (via
      `Path::components`), so a leaf symlink is always classified `OtherFsObject` and refused; `perform_remove`
      re-checks `is_leaf_symlink` before canonicalizing as defence-in-depth against a symlink swapped in after the
      decision.
    - **`HEAD`-resolution routing covers custom pseudorefs.** `WorktreeFileStore::is_per_worktree` (and
      `pointers::is_per_worktree_ref`) route a symbolic-`HEAD` chain that resolves through a one-level pseudoref
      — `AUTO_MERGE`, or any `SOME-REF` matching git's `is_pseudoref_syntax` (top-level, only uppercase letters,
      `_`, and `-`; **digits excluded**, so `CUSTOM-REF` is per-worktree but `CUSTOM1` is shared — verified vs
      stock git) — to the worktree's own admin dir, not the shared common dir. Enumerating only the well-known pseudorefs
      mis-routed the rest to `common`, where they read back as *absent*, so the anchored commit resolved to
      `None` (unborn) and the reachability guard was skipped. (A direct `HEAD → CUSTOM_REF` is a state git itself
      rejects, and gitana's `resolve_ref_terminal` likewise requires `HEAD`'s initial target under `refs/`; the
      gap was only in resolving a pseudoref reached *through* a valid `refs/` chain.)
  - **Split-index `link` bitmaps are decoded bounded by the shared entry count.** The EWAH `bit_size` header is
    attacker-controlled, and a tiny crafted `link` extension could inflate into a ~512 MiB decode / billions of
    `set_bits` positions. `parse_link_extension` now keeps the bitmaps **raw**; `merge_split_index` decodes them
    with `decode_ewah_bounded(.., base.entries.len())` — a valid delete/replace position addresses a shared-index
    entry, so `bit_size` cannot exceed its count, and a crafted header is rejected before allocating.
  - **Sparse *index* (`git sparse-checkout --sparse-index`) is refused honestly, expansion deferred.** A sparse
    index collapses out-of-cone directories into single `040000` sparse-directory entries that gitana does not
    expand, so a status computed over it reports spurious add/delete pairs (a misleading `Dirty`). Removal
    detects such an index (`WorkTree::is_sparse_index`) and refuses *before* the status-derived gates with
    `ProtectionReason::SparseIndexUnsupported` (a conservative unsupported-state refusal, never data loss).
    **Deferred:** expanding sparse-directory entries (reading their trees into skip-worktree blob entries) so a
    clean cone sparse-index worktree removes cleanly — it pulls object reads into the `load_index` hot path, a
    disproportionate change for an exotic, currently-safe case.

  **Still deferred — submodule gitlinks** (mode
  `160000`): `WorkTree::status` reports an (even uninitialised) submodule directory as modified + untracked,
  and `WorkTree::checkout` cannot lay a gitlink down — but this is genuinely part of **submodule support**,
  which gitana does not have (`README.md`), so it stays deferred (a submodule worktree safely over-refuses,
  matching how the CLI already refuses submodule worktrees).
- **Submodule gitlinks in checkout *and* status (slice-2 `create`, surfaced again by slice-3 `remove`).** When
  a commit contains a mode `160000` gitlink, `WorkTree::checkout` (in `gitana-worktree`) fails trying to
  `read_blob` the submodule commit (git creates the empty submodule dir and records the gitlink), **and**
  `WorkTree::status` reports an (even uninitialised, empty) submodule directory as both modified and untracked
  though stock `git status --porcelain` is empty. The status gap now also affects **removal**: routing a
  removal candidate through the status gate makes a *clean* git-authored worktree containing a gitlink refuse
  as `ProtectedWithReason::Dirty` (git's own `worktree remove` would succeed for an uninitialised submodule).
  The behavior is **safe** (a conservative refusal, never data loss) and matches how the CLI already refuses
  submodule worktrees; the real fix is git-faithful gitlink handling in `gitana-worktree`'s checkout/status
  (a cross-crate change the CLI's `worktree add`/`status` need too), so it stays deferred there.
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
