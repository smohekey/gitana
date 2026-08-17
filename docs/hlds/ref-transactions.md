# Ref Transactions

## Context

gitana moves refs through `RefStore::update_ref` / `delete_ref` / `set_symbolic`
(`crates/core/gitana-repository/src/refs.rs`). Each does its work in steps with **no atomic
boundary**: `update_ref` writes the ref (compare-and-set) and *then* appends the reflog;
`delete_ref` mirrors a HEAD-deletion entry, removes the ref, its packed entry, and its own reflog.
Between any two of those steps a failure — a directory/file conflict on a `logs/<ref>` path, a lost
CAS race, a backend I/O error — leaves ref state, reflog state, and the caller's reported outcome
inconsistent.

This surfaced in the receive-pack push-reflog work (merged `e653eb66`, see
`docs/hlds/secure-git-trust-signing.md` neighbours and the `git_receive_reflog.rs` oracle). Codex
review kept finding partial-failure edges, and each ordering fix only relocated the window, because
the underlying primitive is not transactional. An interim receive-pack rollback handles the one
realistic case (a D/F conflict on a branch reflog path); the rest — catastrophic mid-write I/O, a
tight resolve→commit race — cannot be closed without git's **ref-lock transaction** model.

Git's model: a transaction *prepares* by locking every ref it will touch (`<ref>.lock`) and
validating preconditions, then *commits* by writing reflogs and renaming the locked refs into place,
or *aborts* by releasing every lock having changed nothing. Holding the lock across
`read-old → write-reflog → commit-ref` removes the race, and writing the reflog before the ref makes
a reflog failure abort before the ref moves.

Scott chose **full multi-ref push atomicity**. Git-faithfully that is two things: (1) a ref-transaction
layer giving **per-ref** atomicity — which fixes *every* ref mutation (commit, branch, switch, clone,
fetch, reset, merge, trust, worktree, receive-pack), not just push; and (2) the **opt-in `atomic`
push capability** (`git push --atomic`) for all-or-nothing across a push's refs. Git's *default*
receive-pack is **not** all-or-nothing — it reports per-ref `ok`/`ng` — so the per-ref default is
preserved and `atomic` is opt-in.

**No code yet.** This document is the design pass for sign-off before implementation. It lays out the
lock recipe (already validated against the code), the `RefTransaction` architecture, the `atomic`
capability, the slice plan, and the decisions that need Scott's call.

## The lock recipe (validated against the code)

gitana already has every primitive git's ref-lock needs, and a working precedent — the working-tree
`IndexLock` (`crates/core/gitana-worktree/src/worktree.rs`), a capability-token acquired by
`lock_index` (via `write_path_if_absent("index.lock", …)`) and consumed by `commit_index` /
`release_index_lock`. Because Rust has no async `Drop` and releasing a lock is itself an `await`
(`delete_path`), release is an **explicit async method that consumes the token by value** — not RAII.
`RefTransaction` follows exactly this shape.

The `FileStore` primitives (`crates/core/gitana-file-store/src/lib.rs`), confirmed atomic on every
backend — `LocalFileStore` (native cap-std **and** wasm descriptor backends), `MemoryFileStore`,
`WorktreeFileStore` (routing):

- **Acquire** `<ref>.lock` via `write_path_if_absent` — true `O_EXCL`/`create_new` on Local/Worktree,
  atomic under the `RwLock` on Memory. `WriteOutcome::AlreadyExists` ⇒ contended.
- **Commit a move** via `write_path_replace` — atomic temp-write + rename, and *by contract takes no
  internal `<path>.lock`*, so it cannot deadlock against the lock the transaction already holds.
- **Release** via `delete_path("<ref>.lock", None)`.

**The trap:** `write_path_cas` and `delete_path` on Local/Worktree acquire `<path>.lock` internally
(their own cross-process CAS lock). A transaction holding `<ref>.lock` therefore must **not** commit
a *deletion* through `delete_path("<ref>")` — it would spin on `<ref>.lock` (50 × 10 ms) and then
fail "locked by another process". Deletion needs a **lock-free unlink**, the natural sibling of
`write_path_replace`, which Phase 1 adds. (`Memory`'s cas/delete take no lock file, so this is a
Local/Worktree concern only, but the primitive must exist uniformly.)

Using the **git-identical `<ref>.lock` name** (not a private suffix) is deliberate: gitana and stock
git operating on the same on-disk repo then contend on the *same* lock file, so neither stomps a ref
the other is mid-update. `LocalFileStore::list_prefix` already hides `*.lock`, so lock files never
leak into ref listings (Memory does not filter — a test-only divergence to watch).

## Architecture — `RefTransaction`

A new module `crates/core/gitana-repository/src/ref_transaction.rs` (own file, per
`docs/conventions.md`), re-exported from `lib.rs`.

A transaction is built from a set of **ref ops**, each a create/update/delete carrying its
`ReflogIntent`:

```
RefOp { name, expected: Option<Oid>, new: Option<Oid>, reflog: ReflogIntent }
   new = Some ⇒ create/update      new = None ⇒ delete
```

Lifecycle (capability-token, mirroring `IndexLock`):

- **`prepare(files, ops) -> PreparedTxn`** —
  1. Compute the **full lock set**: every op's ref, plus `HEAD` for any op whose ref is the branch
     `HEAD` points at (the split-HEAD reflog cascade / HEAD-deletion entry writes `logs/HEAD`).
  2. Acquire each `<name>.lock` **in canonical sorted order** (lexicographic) — a fixed global order
     makes concurrent transactions deadlock-free.
  3. Under the locks, validate each op's CAS precondition against the *current* value (`expected`),
     and **preflight reflog writability** — that no ancestor of any `logs/<name>` we will write is an
     existing file (the D/F conflict that bit receive-pack). Any failure ⇒ `abort` (release all
     locks) and return the error; nothing is written.
- **`PreparedTxn::commit(self)`** — for each op, in order: write its reflog entries (branch log +
  HEAD cascade, or the HEAD deletion entry), then commit the ref — `write_path_replace` for a move,
  the Phase-1 lock-free unlink for a delete (plus packed-refs removal and best-effort own-reflog
  removal). Then release every lock. Reflog-before-ref + the held lock is what closes the windows.
- **`PreparedTxn::abort(self)`** — release every lock; nothing committed.

`update_ref` / `delete_ref` / `set_symbolic` become **thin wrappers**: build a one-op transaction,
`prepare`, `commit`. **Their signatures are unchanged**, so the ~37 call sites across 16 files
(branch/switch/update-ref/tag/worktree; clone/fetch/prune/rebase/remote/trust; repository
commit/merge/reset; wasm ops) do not churn and inherit atomicity for free. The interim receive-pack
rollback is deleted.

The gating already lives in `RefStore` (`reflog_policy` / `should_log` / the split-HEAD cascade in
`log_ref_update`); the transaction reuses it — this is a *re-plumbing*, not new reflog semantics.

## The `atomic` push capability (Phase 3)

- **Advertise:** add the bare `atomic` token to the `Service::ReceivePack` arm of `base_capabilities`
  (`crates/core/gitana-git-http/src/advertise.rs`), beside `report-status delete-refs`.
- **Parse:** the first-line capability list is currently *discarded* in `parse_command`
  (`crates/core/gitana-git-http/src/receive_pack.rs`, ~line 534). Capture it into a new
  `ParsedRequest` field and detect `atomic`.
- **Apply:** build the trust-cleared, connectivity-checked commands into one `RefTransaction`.
  - **Default (no `atomic`):** commit **per-ref** — each op its own one-op transaction — preserving
    git's independent per-ref `ok`/`ng`, now each atomic.
  - **`atomic`:** `prepare` all ops; if any would be rejected (CAS/non-fast-forward/preflight),
    `ng` the *whole* batch and commit nothing (git's all-or-nothing). Objects still migrate before
    the ref stage, as today.

## Slice plan

- **Phase 0 — reflog slice.** Done, merged `e653eb66` (interim rollback + documented residual).
- **Phase 1 — lock-free delete primitive.** Add `delete_path_unlocked` (name — see Decisions) to
  `FileStore`, sibling of `write_path_replace`: unlink without taking `<path>.lock`. Impl on Local
  (`remove_file`, no `LockFileGuard`), Memory (remove under the write lock), Worktree (route). Extend
  `gitana-file-store-conformance`.
- **Phase 2 — `RefTransaction`.** The module above; rewrite the three primitives as wrappers; remove
  the receive-pack rollback. Unit tests: concurrency (racing writers — one wins, no spurious reflog),
  reflog D/F preflight rejects before mutating, abort leaves nothing, wrappers match prior behaviour.
- **Phase 3 — multi-ref receive-pack + `atomic`. Done.** Advertise/parse/apply as above, plus the
  `gta push --atomic` client. Oracle in a new `git_atomic_push.rs` (a stock-`git` client → bare `git`
  server and a `gta` client → gitana server run the same good-ref + rejected-ref push in parallel):
  `--atomic` leaves both servers' refs untouched, the non-atomic default lands the good ref per-ref —
  byte-identical ref state on both. See the Phase-3 implementation notes below.

Each phase: full workspace green + `cargo fmt` + `codex review --base main` clean, then squash-merged
after Scott's approval.

## Decisions (signed off 2026-07-10)

- **D1 — lock name.** ✓ git-identical `<ref>.lock` (interop-safe, shares the lock with stock git).
- **D2 — lock-free delete name.** ✓ `delete_path_unlocked(path) -> Result<DeleteOutcome>` on
  `FileStore` — mirrors `write_path_replace`'s "caller owns serialisation" contract.
- **D3 — contention policy.** ✓ **retry with backoff** on a held `<ref>.lock` — block briefly (git's
  behaviour; matches the existing `LockFileGuard` 50 × 10 ms) before giving up with `RefLocked`. Note
  for Phase 2: the retry lives in `RefTransaction` (gitana-repository) at the `write_path_if_absent`
  level, so it needs a bounded async backoff there (a small sleep helper; wasm retries immediately, as
  `LockFileGuard::lock_backoff` already does).
- **D4 — orphaned locks.** ✓ A crashed holder orphans `<ref>.lock` (manual removal), exactly as git
  and as `LockFileGuard` already document. No auto-breaking of stale locks in this initiative.
- **D5 — HEAD in the lock set.** ✓ Include `HEAD` when an op cascades to `logs/HEAD`, so a concurrent
  `set_symbolic`(HEAD) cannot interleave.

## Residual / limitations (honest)

- **Commit-phase catastrophic I/O.** After `prepare`'s preflight, the commit still performs real
  writes; a mid-commit backend failure (disk full) on a multi-op transaction can leave earlier ops
  committed. This is git's own practical boundary (it is not a database); `prepare` moves *every
  cheaply-detectable* failure (locks, CAS, D/F) before any mutation, which is what closes the codex
  findings. Not solvable without a WAL, which is out of scope.
- **Reflog-path preflight is advisory across refs.** We hold the *ref* locks, not the `logs/`
  directory tree; a concurrent transaction on an unrelated ref could in principle create a conflicting
  `logs/` directory between our preflight and our write. Vanishingly unlikely and self-corrects on
  retry; noted, not engineered against.
- **Single-writer serving assumption.** The interop/test harness re-opens the repo per request and
  never touches one repo concurrently; the lock makes concurrent writers *correct* regardless, but is
  not exercised under real contention until a persistent multi-connection server exists.

## Implementation notes (as built, Phase 2)

Deltas from the design above and from `codex` review, recorded here rather than rewriting the prose:

- **Realised as `RefStore::transact` + the `RefOp` type, not a standalone `RefTransaction` struct.**
  The prepare/commit engine is inseparable from `RefStore`'s private reflog machinery
  (`reflog_policy` / `should_log` / `log_ref_update` / `append_reflog` / `remove_from_packed`), so it
  lives on `RefStore` in `refs.rs` — co-locating it avoids widening seven helpers to `pub(crate)`.
  `RefOp` (the one caller-facing type) still gets its own file, `ref_op.rs`, per conventions.
- **Directory/file preflight of both the ref path and the reflog path, via a new `FileStore::is_dir`
  (git's prepare-phase ref-name availability check).** Validation checks `path_write_blocked` for a
  move's ref path, (when logged) its reflog path, and (for a cascade) the mirrored `logs/HEAD` —
  `is_dir(target)` catches the destination being a
  directory (a leftover from a nested ref/reflog, *including empty dirs* a delete left behind, which
  `read_path`/`list` miss), and `read_path(ancestor).is_ok()` catches a *file* ancestor (only a file
  reads back `Ok`; a directory or absent path errors, kind varying by backend). `FileStore::exists`
  couldn't do this — it's `metadata`-based and reads `true` for the normal `logs/refs/heads`
  directory. Preflighting both paths for every op means a **validated commit cannot fail on a D/F
  conflict**, so even a multi-op `--atomic` batch is all-or-nothing (the residual is catastrophic
  mid-commit I/O only). A delete needs no preflight — it removes, and validation already proved the
  ref resolves. With both paths validated, commit writes the **reflog before the ref** again (a
  catastrophic `logs/` failure then leaves the ref unpublished — receive-pack's reject-without-moving).
- **Empty ref directories are pruned (git parity), via a new `FileStore::remove_dir`.** Acquiring
  `<ref>.lock` `create_dir_all`s the ref's parents; an aborted transaction (or a delete that empties a
  subtree) would otherwise leave an empty `refs/heads/foo/` that the new `is_dir` preflight reads as a
  permanent conflict. After the transaction's locks release, `prune_empty_dirs` best-effort removes
  now-empty parent directories from the innermost up, stopping at the first non-empty one — exactly as
  git prunes empty ref dirs so a stale directory cannot block a later ref. **It never removes a
  namespace anchor** — a directory of two or fewer path components (`refs`, `refs/heads`, `logs/refs`,
  …), matching git, which skips the first two components; removing `refs/` would strip a directory
  `is_git_dir` requires and make the repository unrecognizable, so deleting the last ref under
  `refs/heads` prunes nothing above it.
- **The HEAD cascade is gated on the prepared flag, *re-confirmed under the lock*.** `HEAD` is read
  once pre-lock to fix the lock set (locked iff some op cascades); `confirm_cascades` re-reads it under
  the acquired locks (catching a `set_symbolic` retarget in the pre-lock→lock window) and keeps the
  cascade only where we hold `HEAD.lock` **and** `HEAD` still points at the branch — so the transaction
  never appends to `logs/HEAD` without holding `HEAD.lock`, nor logs a cascade for a branch HEAD no
  longer tracks.
- **Backoff really waits.** Native sleeps 10 ms on the blocking pool (`spawn_blocking`), so a
  transaction waits out a cross-process `<ref>.lock` holder (stock git) instead of failing instantly —
  mirroring the file store's `LockFileGuard`; `gitana-repository` gains a native-only `tokio` (`rt`)
  dependency for this. wasm is single-process and backs off with a cooperative yield (no runtime timer).
- **`HEAD.lock` routes per-worktree.** `WorktreeFileStore::is_per_worktree` gained `HEAD.lock` (beside
  `index`/`index.lock`) so a transaction retargeting a linked worktree's `HEAD` locks the real
  per-worktree file, interoperably with git.
- **`RefLocked`** is the new `RepositoryError` variant when a `<ref>.lock` stays contended past the
  retries.
- **Native ref mutations are retained through cancellation.** `RefStore::transact` and
  `set_symbolic` first copy their borrowed inputs and clone an aliasing `FileStore::shared_handle`,
  then run lock acquisition, validation, publication, release, and directory pruning in an owned
  Tokio task. Dropping the caller's future stops waiting but does not drop `PathLock` while an
  offloaded filesystem write is still running. Every store handle aliases the same backend,
  temporary-name counter, versions, and in-process locks. Wasm keeps the operation inline because
  its descriptor-backed file-store calls complete synchronously when polled and the component does
  not provide a Tokio runtime.
- **Ref locks are owned RAII paths.** `FileStore::try_lock_path` returns `PathLock`; dropping the
  complete lock set releases every lock synchronously. If acquiring a later lock fails, the
  transaction explicitly drops the locks already acquired and prunes directories created for both
  those names and the failed attempt before returning. This prevents an aborted nested lock such as
  `refs/heads/foo/bar.lock` from leaving an empty `refs/heads/foo/` that blocks the parent ref.
- **Checkout holds `HEAD.lock` end to end (`HeadLock`).** `RefStore::lock_head` returns a `HeadLock`
  (own file `head_lock.rs`) — an owned `HEAD.lock` paired with an aliasing store handle. `gta switch`
  acquires it before it reads the two-tree merge base and holds it through the working-tree mutation,
  then publishes by **consuming** it via `HeadLock::finish_checkout` — never a fresh
  `set_head_symbolic`/`update_ref`, which would try to re-acquire the lock it holds. Because a ref
  transaction moving the branch this worktree's `HEAD` is on must also lock `HEAD` for its reflog
  cascade, the held lock excludes such a move for the checkout's duration — the checked-out branch
  cannot shift under the in-flight checkout. (A cross-worktree move of a branch checked out here does
  not cascade into this `HEAD`, so it is not excluded; that case is out of scope.)
  - **Publish is one consuming, lock-owning operation.** `finish_checkout` optionally creates `branch`
    (git's `switch -c`) and then points `HEAD` at it, both inside a single owned task that owns
    `HEAD.lock` (backed by `RefStore::commit_checkout`). The retained worker owns every lock it relies
    on, so dropping the caller mid-publish cannot release `HEAD.lock` early. The branch is created
    first and `HEAD` retargeted **after**, matching git's create-then-set-HEAD order so the just-created
    branch resolves for `HEAD`'s reflog `old`/`new`.
  - **`switch -c X` when `HEAD` is on an (unborn) `X`.** The create cascades into `logs/HEAD`; because
    the worker owns `HEAD.lock` it writes that split-HEAD reflog directly instead of re-locking `HEAD`
    (which a bare `transact` would do, deadlocking on the held lock). Verified byte-for-byte against git
    2.55 — all three `logs/HEAD` entries in order (`… commit (initial)`, `branch: Created from main`,
    `<tip> <tip> checkout: moving from X to X`).

## Implementation notes (as built, Phase 3)

- **`atomic` advertised beside `report-status delete-refs`** in the `Service::ReceivePack` arm of
  `base_capabilities`.
- **Request-capability parse is one peek.** Both request forms carry the capability list after the
  first NUL of the first pkt-line — a plain command line (`<old> <new> <ref>\0<caps>`) or the
  push-cert marker (`push-cert\0<caps>`) — so a single `requested_atomic(request)` covers signed and
  unsigned pushes, feeding a new `ParsedRequest::atomic`.
- **Apply reuses `RefStore::transact` — no new engine.** `plan_command` turns each command into a
  `RefOp` after applying receive-pack policy (fast-forward unless `force`, deletion only with `force`);
  the default path commits each op as its own one-op transaction (per-ref `ok`/`ng`, unchanged), and
  `atomic` commits every planned op in one `transact` call. `apply_command` is now `plan_command` +
  a one-op `transact`, so the two paths share the policy check.
- **Planning runs *after* `migrate_accepted`, in both modes.** The fast-forward check
  (`is_fast_forward` → `rev_list`) walks the object store, so the pushed commits must be migrated
  first — otherwise an ordinary atomic fast-forward to an existing ref would be rejected for want of
  objects that are still in quarantine. So the order is migrate → plan → `transact`; a `--atomic`
  batch that is then rejected (a non-fast-forward, a denied deletion, a ref named twice, or the
  residual CAS race inside `transact`) `ng`s the whole batch — the offending ref by its reason, the
  rest with git's collateral `atomic push failure` — and **moves no ref**, but (like the existing
  per-ref path) may leave the migrated-but-unreferenced objects behind for gc. Git's quarantine
  discards them instead; matching that is a separate, pre-existing gitana gap, not specific to atomic.
- **Duplicate ref names sink an atomic batch.** git's receive-pack rejects a ref updated twice in one
  push; the atomic path detects a repeated `command.name` up front and fails the batch (the default
  per-ref path already rejects the second update by its now-stale compare-and-set).
- **Client (`gta push --atomic`).** `build_receive_pack_request` / `push_caps` emit the `atomic` token
  only when requested; `push` / `push_signed` first call `ensure_atomic_supported`, which errors
  (git's "the receiving end does not support --atomic push") when the server did not advertise it,
  rather than silently degrading to a per-ref push. The wasm component push is a single ref, so it
  passes `atomic: false` and does not widen the WIT surface.

## Pre-existing smells surfaced (not fixed here, per conventions)

- `write_path_stream_if_absent` on `LocalFileStore` (`lib.rs`) is **not** truly exclusive — it does an
  `exists` check then `rename` (a TOCTOU window), unlike the `create_new`-based `write_path_if_absent`.
  Not on the ref-lock path, but worth a separate fix.
- `Command` in `receive_pack.rs` duplicates `RefUpdate` (already noted in that file as a known smell).
