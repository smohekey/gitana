# Worktree Capability Threading

## Context

Roadmap item 2 from `docs/hlds/wasi-component-porcelain.md`. The `gitana:repo` wasm component
exports the repository *plumbing* set (objects, refs, revisions, `repack`, `init`) over a passed-in
`wasi:filesystem` directory descriptor — no preopens, no ambient authority. The next payoff is
running the working-tree *porcelain* (`commit`, `status`, `merge`, and their siblings) in-component.

The single thing that blocks it: **`gitana-worktree`'s `WorkTree` reaches the filesystem through
ambient `std::fs` + `PathBuf`**, not through a passed-in capability. Everything below `WorkTree`
is already capability-clean — `gitana-repository` (objects, refs, config, and all merge-state
control files: `MERGE_HEAD`/`MERGE_MSG`/`ORIG_HEAD`/…) goes through the `FileStore` abstraction the
component already backs with a descriptor. `WorkTree` is the last ambient-authority seam.

This document is the design pass the roadmap called for. **No code yet** — it lays out the
architecture, the WASI limits that shape it, the slice plan, and the decisions that need Scott's
sign-off before implementation.

## The two filesystem domains

`WorkTree<F, H>` (`crates/core/gitana-worktree/src/worktree.rs:18`) holds a `repo`, a
`work_dir: PathBuf`, and a `git_dir: PathBuf`. It touches the filesystem two distinct ways, and they
want *different* solutions:

### Domain A — the index (under `git_dir`)

`load_index` / `save_index` / `lock_index` / `commit_index` / `release_index_lock`
(`worktree.rs:100–155`) read and write `index` and `index.lock` with raw `std::fs::read`,
`OpenOptions::create_new`, `write_all`, and `rename`.

These files live under `git_dir` — **the exact descriptor the component already holds** (the
`WorktreeFileStore` routes per-worktree paths like `index` to the git-dir store). So Domain A does
not need a new capability at all; it needs to stop bypassing the one that already exists. Redirecting
these five methods onto `self.repo.objects().file_store()` (a `FileStore`) is the minimal change,
and it is enough on its own to run `commit` and `reset --mixed` in-component — neither touches the
working tree.

The one wrinkle is the lock protocol. Today it is git's classic *create-new `index.lock` → hold the
open handle across the mutation → write content → rename over `index`*. `FileStore` is single-shot
(no held handle, and no `rename` in its surface), so the protocol re-expresses as:

- `lock_index` → `write_path_if_absent("index.lock", &[])` — create-new gives the same mutual
  exclusion; `AlreadyExists` → `WorktreeError::IndexLocked`.
- `commit_index` → `write_path_cas("index", bytes, None)` then `delete_path("index.lock")`.
- `release_index_lock` → `delete_path("index.lock")`.

This preserves the observable contract (one writer at a time; a held lock aborts a destructive op
*before* it mutates the working tree) while dropping the "write-through-the-lock-then-rename"
mechanic. `write_path_cas` is itself atomic-replace, so the index is never seen half-written.
(Alternative: extend the `Backend` with a create-new + rename primitive and keep the exact git
mechanic. Rejected in the recommendation below — it adds API surface for no observable gain.)

### Domain B — the working tree (under `work_dir`)

Everything else: `add`/`status`/`diff`/`checkout`/`twoway_merge`/`mv`/`rm`/`restore`. These need a
**second capability** — a directory handle rooted at `work_dir` — and its operation set is *richer*
than `FileStore`'s flat byte API. Inventory of what the working-tree code actually does
(`checkout.rs`, `worktree.rs`, `status.rs`, `diff.rs`, `mv.rs`, `rm.rs`, `restore.rs`, `fsmeta.rs`):

| Operation | `std::fs` today | Callers |
|---|---|---|
| lstat (type + size + mtime/ctime + mode/ino) | `symlink_metadata` | status, diff, checkout, add, mv, rm, restore |
| list a directory (name + entry type) | `read_dir`, `DirEntry::{file_type,metadata,file_name,path}` | status (untracked walk), add (walk) |
| read a file | `read` | add, diff |
| read a symlink target | `read_link` | add, diff, fsmeta |
| read a small text file (`.gitignore`) | `read_to_string` | status, add, checkout |
| write a file | `write` | checkout |
| create a symlink | `os::unix::fs::symlink` | checkout |
| set the exec bit | `set_permissions` (`PermissionsExt`) | checkout |
| make a directory | `create_dir` | checkout (`ensure_parents`) |
| rename | `rename` | mv |
| remove a file | `remove_file` | checkout, rm, restore |
| remove an empty directory | `remove_dir` | checkout (`remove_empty_parents`) |
| remove a directory tree | `remove_dir_all` | checkout (`clear_dest`, dir→file) |

This maps almost 1:1 onto the `wasi:filesystem` descriptor's `*-at` methods (`stat-at` with the
`symlink-follow` path-flag = lstat, `read-directory`, `open-at`, `readlink-at`, `symlink-at`,
`create-directory-at`, `rename-at`, `unlink-file-at`, `remove-directory-at`) — and onto
`cap_std::fs::Dir` natively. That correspondence is why a single capability trait can back both.

## WASI 0.2.12 limits (what shapes the metadata story)

Read from the vendored `crates/wasm/gitana-repo-component/wit/deps/filesystem.wit`. The
`descriptor-stat` record carries only: `type`, `link-count`, `size`, and three `datetime`s
(access / modification / status-change). **There is no permission/mode field, and there is no
chmod-style operation.** Two consequences:

1. **The exec bit is unrepresentable under WASI.** `file_mode` reads `permissions().mode() & 0o111`
   to pick `100755` vs `100644`; `set_mode` chmods on checkout. WASI can do neither, so on wasm a
   regular file is always mode `100644` and the exec bit is silently dropped on checkout. This is
   exactly what git does with `core.fileMode=false`, and exactly what `gitana-worktree`'s existing
   `#[cfg(not(unix))]` fallbacks already do (`fsmeta.rs:56`, `checkout.rs:614`).

2. **`dev`/`ino`/`uid`/`gid` are unavailable**, so the index stat cache degrades to
   `size` + `mtime` + `ctime`. Correctness is preserved: when the stat cache can't confirm a file is
   unchanged, status/diff re-hash the content (`stat_matches` → `blob_of`). This mirrors git's
   `core.checkStat=minimal`, and again matches the existing `#[cfg(not(unix))]` `stat_of`
   (`fsmeta.rs:87`, which fills only `size`).

**Key insight:** the *semantic* degradation is already modeled — by the compile-time `cfg(unix)` /
`cfg(not(unix))` split, and `wasm32-wasip2` is `not(unix)`, so it already inherits the reduced
behaviour. The core of this work is therefore **plumbing, not re-deciding semantics**: replace
`std::fs` free-function calls (anchored on absolute `PathBuf`s) with calls on a rooted capability
handle. See decision D2 for whether to keep the `cfg` split or move the degradation behind the
capability.

Symlinks are *not* a hard WASI limit — `symlink-at` / `readlink-at` exist. The open question (D3) is
whether to wire real WASI symlinks or fall back to the `#[cfg(not(unix))]` "write the target as
regular-file content" behaviour. The sandbox host may still reject a symlink whose target escapes
the descriptor root; git blobs can hold arbitrary targets, so some will fail closed regardless.

## Proposed architecture

Mirror the proven object-DB pattern (`FileStore` async top tier over a sync `Backend` where the
cap-std-vs-wasi split lives, both impls behind one handle, sync→async via the existing `blocking()`
shim), but for a *tree-and-metadata* interface instead of a flat byte store.

### A new capability trait — `WorkDirFs`

A synchronous, path-addressed directory capability rooted at the working tree. Sync, because (a) the
low-level `Backend` it parallels is sync, (b) the wasi descriptor calls are themselves synchronous
(positional I/O, no `wasi:io` pollables — the same reason the component's `block_on` is sound), and
(c) the existing worktree helpers already call blocking `std::fs` inside their `async fn`s, so a sync
capability changes nothing about the current (im)purity. Shape (illustrative, not final):

```rust
pub trait WorkDirFs: Send + Sync + 'static {
    fn lstat(&self, path: &str) -> io::Result<Option<Meta>>;   // None = absent
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;
    fn read_link(&self, path: &str) -> io::Result<Vec<u8>>;    // raw target bytes
    fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry>>; // (name, type)
    fn write(&self, path: &str, bytes: &[u8], exec: bool) -> io::Result<()>;
    fn symlink(&self, target: &[u8], path: &str) -> io::Result<()>;
    fn create_dir(&self, path: &str) -> io::Result<()>;
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    fn remove_file(&self, path: &str) -> io::Result<()>;
    fn remove_dir(&self, path: &str) -> io::Result<()>;
    fn remove_dir_all(&self, path: &str) -> io::Result<()>;
}
```

`Meta` is a capability-neutral metadata struct — `kind` (file/dir/symlink), `size`, `mtime`/`ctime`
(sec+nsec), and *optional* `mode`/`ino`/`dev`/`uid`/`gid`. cap-std fills the full set; the wasi impl
fills what `descriptor-stat` provides and leaves the rest `None`. `fsmeta.rs`'s `file_mode` /
`stat_of` / `mode_of` derive the git mode and index `Stat` from `Meta`, so the exec-bit / stat-cache
degradation follows the *capability*, not the compile target (see D2).

Paths are work-tree-relative, `/`-separated, lexically validated (reuse `checkout::validate_path` and
the existing `resolve` discipline); confinement is structural — cap-std `Dir` and the wasi
`Descriptor` each resolve against themselves and reject escapes at the syscall boundary. The
CVE-class guards (`has_symlinked_ancestor`, `ensure_parents` refusing symlinked ancestors) stay,
re-expressed on `lstat`.

### Two impls, in `gitana-file-store-local`

That crate already carries the exact deps and cfg discipline this needs: `cap-std` (native-only,
because its WASI deps don't build on stable), `wasip2` (for wasm), and the `blocking()` sync/async
shim. And `gitana-worktree` **already depends on it** — no new dependency edges.

- `CapWorkDir { dir: cap_std::fs::Dir }` — native; `dir.read`, `dir.symlink_metadata`, `dir.entries`,
  `dir.write`, `dir.symlink`, `dir.set_permissions`, `dir.rename`, `dir.remove_dir_all`, …
- `DescriptorWorkDir { dir: wasip2::filesystem::types::Descriptor }` — wasm; the `*-at` methods,
  exactly as `descriptor_backend.rs` already does for the object store (including per-component
  `create-directory-at` walking, since WASI has no multi-component mkdir).

Construction stays capability-pure, mirroring `LocalFileStore::{from_dir, from_descriptor}`:
`from_dir(Dir)` native, `from_descriptor(Descriptor)` wasm. The one native edge that mints ambient
authority from a path is `gta-core` (`repo.rs`, alongside the existing
`Dir::open_ambient_dir` for the git dir).

### Threading it into `WorkTree`

`WorkTree` swaps `work_dir: PathBuf` for the capability handle, held as a **new generic parameter**
`WorkTree<F, W, H>` where `W: WorkDirFs` (decision D1, resolved: generic, not a boxed field — keeps
the zero-cost, convention-consistent threading the crate uses for `F: FileStore`). The helper modules
(`checkout`/`status`/`diff`/`mv`/`rm`/`restore`) take `wt: &WorkTree<F, H>` today and reach the
filesystem via `wt.work_dir().join(p)` + `std::fs::X`; they change to `wt.work().X(p)`, and every
`impl<F, H>` / free-fn signature across those files grows the `W` parameter. This is a large but
mechanical diff — bounded and well-covered by the existing test suite.

`git_dir` stays as-is for now, but its *file access* (Domain A) routes through the `FileStore`, so
the remaining `git_dir: PathBuf` becomes vestigial for I/O (it may still be read for path reporting
like `git_dir()`), and can be dropped once nothing reads it.

## Slice plan

Each slice is its own worktree + branch, codex-reviewed to clean before merge, with the existing
native test suite as the behavioural oracle (no behaviour change until the wasm slices). Verification
gate per the handoff: `cargo build/test --workspace` · the `--target wasm32-wasip2` check set ·
`cargo fmt --all -- --check` · `RUSTDOCFLAGS="-D warnings" cargo doc`.

1. **Index → `FileStore` (Domain A).** Redirect the five index methods onto
   `repo.objects().file_store()`; re-express the lock protocol on CAS primitives (D4). No new
   capability, no working-tree change. Unblocks `commit` and `reset --mixed` structurally. Small,
   fully native-testable, self-contained win.

2. **`WorkDirFs` trait + `CapWorkDir` + read paths.** Define the trait and the native impl; refactor
   `WorkTree` to hold the capability (D1); thread the **read-only** helpers — `fsmeta` (`blob_of` /
   `stat_of` / `mode_of` / `file_mode` deriving from `Meta`), `status`'s untracked walk, `diff`.
   Native-only, zero behaviour change; the full status/diff test suite is the oracle.

3. **Write paths.** Thread the **mutating** helpers — `checkout` (`write_worktree_file`,
   `ensure_parents`, `clear_dest`, removal, symlink, exec bit), `add`'s directory walk + staging,
   `mv` rename, `rm` / `restore` removal. Still native-only; checkout/add/mv/rm/restore/merge tests
   are the oracle. After this, `gitana-worktree` has **no ambient `std::fs`**.

4. **`DescriptorWorkDir` (wasi impl) + the third descriptor.** Implement the trait over a
   `Descriptor`; extend `open-worktree` (and add a work-dir-bearing `open`) in `porcelain.wit` to
   accept the work-tree descriptor; construct the `WorkTree` capability from it in the guest. Host
   e2e proves a round-trip in both hash formats, native gitana as oracle.

5. **Porcelain exports.** Add `gitana-worktree` + `gitana-porcelain` deps to the component; expose
   `commit`, `status`, and `merge` (plus the `add`/`checkout` they need) as component exports with
   typed WIT records/variants. This is where the payoff lands — the whole point of items 1–4.

Slices 1–3 have value on their own (they remove the `WorkTree` ambient-authority seam and the
`cfg(unix)` metadata smell) even before the wasm slices land, and they are independently reviewable.

## Decisions

- **D1 — threading style. RESOLVED: new generic parameter `WorkTree<F, W, H>`** (`W: WorkDirFs`).
  Zero-cost and consistent with how the crate threads `F: FileStore`. Cost: every `impl<F, H>` /
  free-fn signature across `checkout`/`status`/`diff`/`mv`/`rm`/`restore` grows a `W` param (a large
  but mechanical diff). *(Considered and rejected: an `Arc<dyn WorkDirFs>` field — smaller ripple but
  dynamic dispatch and a convention deviation.)*

- **D2 — degradation source. RESOLVED: derive git mode / index `Stat` from capability-reported
  `Meta`**, not the compile-time `cfg(unix)` split. Native-through-capability keeps real exec bits
  and inode data; wasm degrades where `descriptor-stat` is silent. Removes the `cfg` smell; `fsmeta`
  moves from `cfg` branches to `Option` handling on `Meta`.

- **D3 — symlinks on wasm. RESOLVED: real WASI symlinks** via `symlink-at` / `readlink-at`, keeping
  blob round-trip fidelity. A sandbox may reject targets that escape the descriptor root — those fail
  closed, surfaced as an error.

- **D4 — index lock. Proposed (accepted unless flagged): re-express on `FileStore` CAS primitives**
  (`write_path_if_absent` + `write_path_cas` + `delete_path`) rather than extending the `Backend`
  with create-new + rename. No new API surface; the observable contract is preserved.

- **D5 — crate placement (minor). Proposed (accepted unless flagged): trait + both impls in
  `gitana-file-store-local`** (already holds the cap-std / wasip2 deps and the sync/async shim, and
  is already a dependency of `gitana-worktree`). Alternative: a dedicated `gitana-worktree-fs` crate.

## Pre-existing smells surfaced (not fixed here, per conventions)

- The worktree helpers already call blocking `std::fs` inside `async fn`s (e.g. `checkout::run`'s
  `std::fs::write`), which technically violates the "never block the runtime" convention. The
  capability keeps that shape (sync calls in async fns). Worth a separate pass to offload via
  `spawn_blocking` if it ever matters natively; it does not affect the wasm goal.
- `FileStore`'s trait doc still calls it `GitFileStore` in places
  (`gitana-file-store/src/lib.rs:4,35,64`) — stale naming.
- `gitana-worktree`'s `RmOutcome` has a pre-existing `cargo doc` intra-link break (noted in the
  handoff), unrelated to this work.
