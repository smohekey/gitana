# WASI-Component File Store For Gitana's Object Database

## Context

Gitana's core is a clean-room, SHA-256-native Git implementation: object codecs, content-addressed
storage, refs, revision walking, working-tree/index behaviour, and Smart-HTTP protocol machinery,
all in-process. This design makes the **object-database layer compile to and run as a
`wasm32-wasip2` component** — a WebAssembly component that receives filesystem access as a
*capability* granted by its host, rather than reaching for ambient authority (absolute paths, a
process-wide current directory).

Most of the stack was already WASI-clean; the sole obstacle was how the local file store touches
the disk:

| Concern | State before this work | WASI-ready? |
|---|---|---|
| Hashing | `sha1`, `sha2`, `hmac` (RustCrypto, pure Rust) | Yes |
| Compression | `flate2` → `miniz_oxide` + `crc32fast` (pure Rust, no C zlib) | Yes |
| Async model | `FileStore` uses native `async fn` (no `async_trait`); current-thread tokio | Yes |
| Object store | pack cache on `tokio::sync::Mutex`; async `ByteReader` (`tokio::io::AsyncRead`) | Yes |
| Local file store | `tokio::fs`, `canonicalize`, `std::process::id()`, absolute-path root | **No** |
| Working tree | 60+ direct `std::fs` calls (index/status/add/checkout) | Out of scope |
| Remote transport | `reqwest` → `rustls` → `ring` (C crypto), `tokio::net` | Out of scope |

The unit of work here is `gitana-file-store-local` (the `FileStore` backend) and the ripple its
constructor change causes. The working tree and networking are explicit non-goals (see below).

## Constraint: cap-std Does Not Build For wasip2 On Stable

The natural tool for a capability-based file store is
[`cap-std`](https://github.com/bytecodealliance/cap-std): every operation goes through a
`cap_std::fs::Dir` handle, so a path can never escape the directory. cap-std is the foundation of
Wasmtime's own WASI filesystem implementation.

The async-native sibling `cap-async-std` was evaluated and **rejected**: it is built on `async-std`,
which was discontinued ([RUSTSEC-2025-0052](https://rustsec.org/advisories/RUSTSEC-2025-0052)); it
would drag a second, dead runtime alongside tokio; and on single-threaded wasip2 its async-ness buys
nothing (there is no reactor to free and no thread pool to offload to).

Building against synchronous cap-std then surfaced the load-bearing fact:

> **cap-std 4.0.2 (latest) cannot compile for `wasm32-wasip2` on any current Rust toolchain.**

Its transitive dependency `io-lifetimes 3.0.1` uses `std::os::wasi::io::{AsFd, …}`, which is gated
behind the unstable `wasip2` library feature and is *not declared* by that crate, so it fails
identically on:

| Toolchain | Result |
|---|---|
| stable 1.96 | `error[E0658]: use of unstable library feature 'wasip2'` |
| nightly 1.98 (latest) | same — `std::os::wasi::io` is still unstable |
| `RUSTC_BOOTSTRAP=1` on stable | same — permits `#![feature]`, cannot *declare* the missing feature |
| `wasm32-wasip1` | `error[E0554]: #![feature] on stable` (via `io-extras`'s `wasi_ext`) |

The cap-std family targets nightly for WASI and has not reached stable wasip2. Everything *else* in
the object-DB layer already compiles to a stable wasip2 component; cap-std is the only blocker.

**Decision:** use cap-std where it works (native) and `std::fs` where it does not (wasm), behind a
single file-store implementation. On wasip2, WASI *preopens* provide the capability that cap-std's
`Dir` provides on native — the host confines the component to the granted directory, so the security
property (no ambient escape) holds on both targets by different mechanisms.

## Design: One FileStore, Two Backends

`LocalFileStore` keeps the store's semantics in one place and swaps only the raw filesystem
primitives at compile time, behind an internal `Backend` trait:

```rust
trait Backend: Send + Sync + 'static {
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;
    fn read_range(&self, path: &str, offset: u64, length: u64) -> io::Result<Vec<u8>>;
    fn create_dir_all(&self, path: &str) -> io::Result<()>;
    fn create_new(&self, path: &str) -> io::Result<Option<Box<dyn Write + Send>>>; // None = existed
    fn open_read(&self, path: &str) -> io::Result<Box<dyn Read + Send>>;           // streaming source
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    fn remove_file(&self, path: &str) -> io::Result<()>;
    fn exists(&self, path: &str) -> io::Result<bool>;
    fn list_names(&self, dir_rel: &str) -> io::Result<Vec<String>>;
}
```

| | native (`CapBackend`) | wasm (`StdBackend`) |
|---|---|---|
| Backing | `cap_std::fs::Dir` capability | `std::fs` rooted at a `PathBuf` |
| Sandbox | the `Dir` — structural, per-component, no TOCTOU | WASI preopen — enforced by the host runtime |
| Path form | `Dir`-relative | `root.join(rel)`, resolved through the preopen table |
| Availability | `cfg(not(target_arch = "wasm32"))` | `cfg(target_arch = "wasm32")` |

The trait is object-safe; `LocalFileStore` holds `Arc<dyn Backend>`. The atomic-write, content-hash
CAS, per-path locking, temp-file naming, and streaming logic are written **once** against `Backend`.

### The async facade

cap-std and `std::fs` are synchronous; the `FileStore` trait is async. Every operation runs through
one helper so the async contract holds without blocking the runtime:

```rust
#[cfg(not(target_arch = "wasm32"))]           // native: keep the reactor free
async fn blocking<T, F>(f: F) -> T where F: FnOnce() -> T + Send + 'static, T: Send + 'static {
    tokio::task::spawn_blocking(f).await.expect("file-store blocking task panicked")
}
#[cfg(target_arch = "wasm32")]                // wasm: single-threaded, blocking is the norm
async fn blocking<T, F>(f: F) -> T where F: FnOnce() -> T { f() }
```

This is the same cost `tokio::fs` already paid (its "async" fs is `spawn_blocking` over `std::fs`),
so native characteristics are unchanged; wasm resolves eagerly with no thread pool.

## Capability Boundary

The crate is **capability-pure**: it never calls `ambient_authority()`. The only constructor takes
an already-open capability, so authority is minted at the program edge, not in the library:

```rust
#[cfg(not(target_arch = "wasm32"))] pub fn from_dir(dir: cap_std::fs::Dir) -> Self;  // native
#[cfg(target_arch = "wasm32")]      pub fn from_root(root: impl Into<PathBuf>) -> Self; // wasm preopen
```

- **Native edge** (`gta-core`): repository discovery (the ambient walk up to `.git`) already lives
  at the CLI edge; it opens `<git_dir>` and `<common_dir>` as `Dir`s there and hands them in.
  `open_generic` now returns `io::Result` because opening a `Dir` can fail. Every ambient open is
  greppable via `ambient_authority()` — a handful of audited call sites, which aligns with the
  secure-git trust posture.
- **Native tests**: each opens its own `Dir` (cap-std is a native-only dev-dependency).
- **wasm host**: `wasmtime run --dir <host>::/store …` preopens `/store`; the guest builds
  `LocalFileStore::from_root("/store")`. No ambient authority is exercised at all.

`WorktreeFileStore` (linked-worktree routing) composes two `Dir`s and is native-only — the wasm
target uses a single `LocalFileStore`.

## Filesystem Semantics (written once)

- **Immutable writes** (`write_path_if_absent`) — `create_new` (atomic refuse-if-exists).
- **Conditional writes** (`write_path_cas`) — a content-hash `Version` (SHA-256 hex) guarded by a
  per-path in-process `tokio::sync::Mutex` (native) *and* a `<path>.lock` file for cross-process
  exclusion (like git's ref locks). On wasm the in-process lock is compiled out — the CAS
  read-compare-write runs inside one synchronous `blocking` call with no interleaving await point,
  so it is atomic with respect to other tasks.
- **Atomic publish** — write to a temp file, then `rename` into place, so a partial write never
  appears at the destination.
- **Temp-file names** — `.tmp.<n>` from a shared monotonic `AtomicU64`, created with `create_new`
  and retried on collision. The counter is **seeded per process from the wall clock** so a fresh
  process does not reprobe a crashed one's `.tmp.<n>` window; this replaces `std::process::id()`
  (meaningless under wasip2) and is safer than a reusable pid because the clock never repeats.
- **Path validation** — a cheap lexical check rejects `..`, `.`, empty, and NUL components (a
  deterministic `Backend` error and defence-in-depth); the sandbox itself is enforced by the `Dir`
  (native) or the preopen (wasm), so the previous `canonicalize`-based escape check is gone.

## Streaming Over A Synchronous Backend

`read_path_stream`/`write_path_stream_if_absent` deal in `tokio::io::AsyncRead`, which has no
zero-cost bridge to a synchronous file. They stream for real (no whole-value buffering), cfg-split:

- **Reads** — `open_read` yields a sync reader. Native spawns a blocking-pool pump that sends 64 KiB
  chunks over a bounded channel, wrapped as an `AsyncRead` (`ChannelReader`) with a small leftover
  buffer; wasm reads inline in `poll_read` (`InlineReader`).
- **Writes** — the async source is drained a chunk at a time straight to the temp file (the handle
  moves through `blocking`), enforcing `max_len` as it goes, then flushed and renamed.

## What Changed

| Crate | Change |
|---|---|
| `gitana-file-store-local` | `Backend` trait + `CapBackend`/`StdBackend`; `from_dir`/`from_root`; streaming; temp-seed; cap-std/tokio gated native-only |
| `gta-core` | `open_generic` opens `Dir`s at the edge, returns `io::Result`; `?` threaded through dispatch + `init`/`clone`/`fetch`/`push`/`pull` |
| `gitana-worktree`/`-repository`/`-porcelain` | tests build the native store via `from_dir`; cap-std dev-dep gated native-only; native-only test suites `cfg`-excluded from wasm |
| workspace | `cap-std` added to `[workspace.dependencies]` |
| `crates/demo/wasm-object-db` | new: a wasip2 binary that round-trips one blob object; runs on native and under wasmtime |

## Verification

- Native: `cargo build --workspace` and `cargo test --workspace` green; `cargo fmt --all -- --check`
  clean; `unsafe_code` remains forbidden.
- wasm: `cargo build --target wasm32-wasip2` and `cargo check --target wasm32-wasip2 --all-targets`
  clean across every wasm-capable crate (object, file-store, file-store-memory, object-store,
  file-store-local, repository, worktree, porcelain, demo); `cargo doc -D warnings` clean.
- End-to-end: `wasmtime run --dir …::/store target/wasm32-wasip2/debug/wasm-object-db.wasm` writes a
  loose SHA-256 object under the host preopen and reads it back, byte-identical to the native run.
- The `gitana-file-store-conformance` contract, the hardening suite (path-traversal, symlink-escape,
  concurrent-CAS), and the streaming checks all pass on the native cap-std backend.

## Non-Goals And Follow-Ups

Out of scope for this slice:

- The **working tree** (`gitana-worktree`): its 60+ direct `std::fs` calls still assume ambient
  authority. Threading a capability root through `add`/`status`/`checkout`/`diff` is the next slice
  if in-wasm work-tree operations are wanted. Note: WASI symlink creation and the executable bit are
  limited and would need validation there.
- **Networking** (`gitana-remote`/`gitana-porcelain` transport): `reqwest` → `ring` does not build
  for wasip2, and `tokio::net` is unavailable. A wasm target would need an HTTP transport trait over
  a `wasi:http` host import. `gta` (the CLI) is native-only for the same reason.
- **True WIT/component authoring**: the demo is a wasip2 target binary (already a component on
  current toolchains); a first-class component with a hand-written `wasi:` world and a reusable
  host-embedding crate is future work.

Follow-ups worth tracking:

- The streaming write issues one `spawn_blocking` per 64 KiB chunk (correct and bounded-memory); a
  single long-lived pump task fed over a channel would cut per-chunk handoff for very large packs.
- If/when cap-std's WASI dependencies build on stable (or WASIp3 lands native async), the wasm
  `StdBackend` could be reconsidered in favour of a uniform cap-std capability model.
