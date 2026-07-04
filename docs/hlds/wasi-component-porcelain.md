# Descriptor-Capability Component Exports For Gitana's Repository Layer

## Context

The previous WASI slice (`docs/hlds/wasi-capstd-file-store.md`) made the object-database layer
compile to `wasm32-wasip2`, but its wasm story was a *command binary with a preopen-path
convention*: the host runs `wasmtime run --dir <host>::/store …` and the guest hardcodes
`LocalFileStore::from_root("/store")`. That is capability-*confined* but not capability-*passed* —
the path `"/store"` is an out-of-band contract between host and guest, resolved through the WASI
preopen table by `std::fs`.

This spike proves the boundary gitana actually wants: **library-style component exports that
receive a `wasi:filesystem` directory *descriptor* as an argument**. The descriptor is the
capability. There is no preopen table lookup, no agreed-upon path string, no `std::fs`, and no
ambient authority anywhere in the guest. The exports follow the porcelain pattern (typed
records/variants in and out, no rendering), so the same surface can grow toward the full
`gitana-porcelain` op set.

Verified toolchain facts (July 2026) that shaped the design:

- **cap-std still does not build for `wasm32-wasip2` on stable** (io-lifetimes 3.0.1 uses the
  unstable `wasip2` std feature; untracked upstream). Avoiding cap-std on wasm remains necessary.
- The **`wasip2` crate (1.0.4+wasi-0.2.12)** provides stable-Rust guest bindings for WASI 0.2,
  including `filesystem::types::Descriptor` with the full method surface — the wasm-native
  equivalent of `cap_std::fs::Dir`.
- **wit-bindgen 0.58** + plain `cargo build --target wasm32-wasip2` is the blessed flow
  (cargo-component is being deprecated); the wasip2 target emits a real component directly.
- **wasmtime/wasmtime-wasi 46** exposes the host-side building blocks
  (`filesystem::Dir::new(cap_std::fs::Dir, …)`, `Descriptor::Dir`, `ResourceTable::push`).
- **WASI 0.3** (native component-model async) shipped 2026-06-11, but the Rust guest target is
  Tier 3 and wasmtime's p3 support is marked experimental — this spike targets 0.2 and keeps the
  0.3 migration in view (see Roadmap).

## What Was Built

```
crates/wasm/gitana-repo-component    wasm32-wasip2 reactor component (cdylib)
  wit/porcelain.wit                  package gitana:repo@0.1.0, world `repo`
  wit/deps/{filesystem,io,clocks}    vendored WASI 0.2.12 WIT
crates/wasm/gitana-repo-host         wasmtime 46 host harness + e2e tests
```

The world exports a `repository` resource:

```wit
resource repository {
    open: static func(git-dir: descriptor) -> result<repository, repo-error>;
    hash-kind: func() -> hash-kind;                                  // sha1 | sha256
    read-commit: func(spec: string) -> result<commit-info, repo-error>;
    write-blob: func(data: list<u8>) -> result<string, repo-error>;
}
```

- `open` takes an **owned** descriptor: a resource cannot retain a call-scoped borrow, and owned
  transfer is the natural capability semantic ("here is your git dir"). The host keeps its own
  handle by minting the guest's descriptor from a `try_clone()`d/`open_ambient_dir`'d `Dir`.
- The hash algorithm is a runtime fact detected **through the descriptor** at `open`
  (`gitana_repository::detect_hash_kind`, reading `config` via the `FileStore`), then dispatched
  to the compile-time `H` exactly like `gta-core`'s dispatch — a two-arm enum in the guest.
- `read-commit` + `write-blob` are the smallest pair exercising both the read path (HEAD →
  symbolic ref → loose ref → object inflate, abbrev lookup via `list_prefix`) and the write path
  (`create_dir_all` → `create_new`-exclusive temp file → atomic `rename` publish).

## The Descriptor-Passing Recipe

This is the part with no published end-to-end example; it worked exactly as constructed.

**Guest** — the world imports `wasi:filesystem/types@0.2.12`; `wit_bindgen::generate!` remaps
every imported wasi interface onto the `wasip2` crate:

```rust
wit_bindgen::generate!({
    path: "wit", world: "repo",
    with: {
        "wasi:filesystem/types@0.2.12": wasip2::filesystem::types,
        "wasi:io/error@0.2.12":         wasip2::io::error,
        "wasi:io/poll@0.2.12":          wasip2::io::poll,
        "wasi:io/streams@0.2.12":       wasip2::io::streams,
        "wasi:clocks/wall-clock@0.2.12": wasip2::clocks::wall_clock,
    },
});
```

The remap is the type-unification linchpin: the export's `git-dir` parameter *is*
`wasip2::filesystem::types::Descriptor` — the same concrete type
`LocalFileStore::from_descriptor` takes — so the granted handle flows from the export straight
into the file store with zero bridging and zero unsafe in gitana code. (wit-bindgen 0.58
generating against types produced by wasip2's wit-bindgen 0.57 is fine: both lower to the same
canonical ABI.)

**Host** — `bindgen!` remaps wholesale, and a descriptor is minted per granted directory:

```rust
wasmtime::component::bindgen!({
    path: "../gitana-repo-component/wit", world: "repo",
    imports: { default: async }, exports: { default: async },
    with: { "wasi": wasmtime_wasi::p2::bindings },
});

let dir = cap_std::fs::Dir::open_ambient_dir(git_dir, ambient_authority())?; // host edge
let dir = wasmtime_wasi::filesystem::Dir::new(dir, DirPerms::all(), FilePerms::all(),
    OpenMode::READ | OpenMode::WRITE, false);
let handle: Resource<Descriptor> = store.data_mut().table.push(Descriptor::Dir(dir))?;
repo.gitana_repo_porcelain().repository().call_open(&mut store, handle).await??;
```

The `WasiCtx` is built with **no preopens** — instantiation succeeds and the guest operates purely
on the passed descriptor, which is itself part of the proof.

wasmtime-46 gotchas encountered: `wasmtime::Error` is no longer anyhow (import
`wasmtime::error::Context`); `Config::async_support` is deprecated (async is always on);
wasmtime-wasi 46 is built against **cap-std 3.x** while the gitana workspace uses 4.x — the host
crate names the compatible major under a renamed dep (`cap-std-host = { package = "cap-std",
version = "3.4" }`) for the `Dir` handed to `filesystem::Dir::new`.

## DescriptorBackend

`gitana-file-store-local` gained a third impl of its private `Backend` trait (wasm-only,
alongside `StdBackend`), plus `LocalFileStore::from_descriptor(Descriptor)`:

| `Backend` method | `wasi:filesystem` mapping |
|---|---|
| `read` / `read_range` | `open-at(symlink-follow, …, read)` + positional `read` loop (64 KiB requests, eof-flag terminated) |
| `create_dir_all` | per-component `create-directory-at` (`exist` = ok; creation is single-segment even though `open-at` resolves whole paths) |
| `create_new` | `open-at(create \| exclusive, write)`; `exist` → "already there"; wrapped in an offset-tracking `Write` adapter |
| `open_read` | `open-at(read)` → offset-tracking `Read` adapter (feeds the existing wasm `InlineReader`) |
| `rename` | `rename-at(from, self, to)` |
| `remove_file` | `unlink-file-at` |
| `exists` / `size` | `stat-at(symlink-follow)` (`no-entry` → false) |
| `list_names` | `read-directory` → `read-directory-entry` loop (`no-entry` on open → empty, matching the other backends) |

Notes:

- `wasip2`'s `Descriptor` wraps an `AtomicU32` handle and is structurally `Send + Sync`, so the
  existing `Backend: Send + Sync` bounds and `Box<dyn Write + Send>` hold unchanged.
- All positional I/O — no `wasi:io` streams, no pollables. That keeps the whole backend
  synchronous and underpins the async story below.
- Everything above the `Backend` seam (temp+rename atomic publish, content-hash CAS, `.lock`
  files, path validation, streaming) is untouched — the seam did exactly the job it was built for
  in the previous slice.
- `StdBackend`/`from_root` have since been **deleted**: `wasm-object-db` takes its `/store`
  preopen as a *descriptor* via `wasi:filesystem/preopens#get-directories` →
  `from_descriptor`, so the descriptor backend is the only wasm backend.

## The Async Story (WASI 0.2)

WASI 0.2 exports are synchronous; gitana's engine is async-first. The export shim drives each op
with a noop-waker poll loop (`block_on`), which is sound **because nothing in this path can park**:
every engine `.await` bottoms out in (a) the file store's wasm `blocking()` helper — inline,
immediately `Ready`; (b) an uncontended single-task `tokio::sync::Mutex` in the pack cache; or
(c) the synchronous descriptor reader. No `wasi:io` pollable is ever awaited. A poll-count
bail-out turns any future violation of that invariant (e.g. a `wasi:http` transport) into a trap
with a message instead of a silent spin. WASI 0.3's native async exports would delete this shim
entirely — it is the single piece of scaffolding 0.3 obsoletes.

## Verified

- `cargo test -p gitana-repo-host` — the e2e proof, per hash format (sha1 **and** sha256):
  component instantiated with no preopens; one granted descriptor; in-guest hash detection;
  `read-commit(HEAD)` equal field-for-field to the native oracle; `write-blob` returning the
  natively-computed id, idempotent on repeat, and byte-identical when read back natively; unknown
  specs produce typed errors, not traps. (The test builds the guest itself: `cargo build -p
  gitana-repo-component --target wasm32-wasip2` behind a `OnceLock` — no build.rs, no
  target-dir-lock deadlock.)
- `cargo build --workspace` / `cargo test --workspace` green; `cargo fmt --all -- --check` clean;
  `cargo check --target wasm32-wasip2 --all-targets` clean for every wasm-capable crate including
  the component; `unsafe_code` stays forbidden in every gitana crate — the sole exception is the
  component crate itself, which deliberately does not inherit workspace lints because
  `wit_bindgen::generate!` expands to canonical-ABI glue containing `unsafe` (documented in its
  Cargo.toml).
- `gitana-repository` gained `Config::read(store)` + `detect_hash_kind(store)` with unit tests
  over the memory store; `Repository::open` now reuses `Config::read`.

## What The Spike Proves / Does Not Prove

Proves: descriptor-passing exports work on today's stable toolchain end-to-end; the `Backend`
seam absorbs a third capability model without touching store semantics; runtime hash dispatch
works in-guest through the capability; guest-written objects are bit-exact.

Does not prove: worktree ops (still ambient `std::fs`), pack-heavy repos at scale under wasm,
cross-process lock behavior under wasm hosts, networking, or concurrent multi-store access from
one component instance.

## Addendum: The 0.2.0 Surface (roadmap item 1, done)

`gitana:repo@0.2.0` grew the `repository` resource to the full repo-level plumbing set — the
`gta` `on_repo` command set minus prune/gc:

- **Reads**: `read-object` (kind + canonical payload), `read-blob`, `read-commit`, `read-tag`,
  `ls-tree` (recursive, via the new `Repository::peel_to_tree`), `read-config` (raw text).
- **Revisions**: `rev-parse`, `rev-list(tips, max-count)` (ids, newest-first), `merge-base`
  (empty = no common ancestor, data not error), `is-ancestor`.
- **Refs**: `list-refs(prefix)` (packed-refs merged, loose wins), `head` (unborn / symbolic /
  detached), `resolve-ref`, CAS `update-ref`/`delete-ref` (`ref-moved` on mismatch; delete
  rewrites `packed-refs`), `read`/`set-symbolic-ref`.
- **Writes**: `write-blob`, `write-tree` (`file-mode` enum input), `create-commit` (specs
  strictly kind-checked; raw identity lines).
- **Maintenance**: `repack(geometric)` honoring `pack.packSizeLimit` (geometric factor 2, as
  `gta`).
- **`init(git-dir, kind)`**: the one export where the algorithm is *chosen*, not detected —
  lays out git's empty skeleton (via the new `LocalFileStore::create_dir_all`), writes
  `config`/`HEAD` idempotently, and refuses a different-format re-init with
  `unsupported-format`.
- **Errors**: `repo-error` is now `not-found | unknown-revision | ambiguous | invalid |
  ref-moved | unsupported-format | corruption | backend`, backed by a core split of
  `RepositoryError::InvalidRef` into `UnknownRevision`/`AmbiguousRevision`.

Findings from this slice:

- **In-guest pack encoding needs a bigger stack**: debug builds materialize miniz_oxide's
  ~300 KiB compressor state on the wasm shadow stack deep inside the repack chain, blowing the
  1 MiB default (a `0xffff…` stack-underflow memory fault). The component links with
  `-zstack-size=8MiB` (build.rs), matching native thread stacks.
- Post-repack reads through packs + the multi-pack-index work unchanged through the descriptor
  backend — the spike had only proven loose objects.
- Abbreviated-id resolution scans **loose objects only** (`objects/xx/` prefix listing); after a
  repack, abbreviations of packed objects do not resolve. Engine limitation, not a component
  one; noted for a future engine slice.
- prune/gc remain excluded: their root collection must include the worktree *index* (staged
  objects), which is ambient-`std::fs` territory until worktree threading lands.

## Roadmap

1. ~~**Full repo-level WIT surface**~~ — done, see the addendum above.
2. ~~**Two-descriptor `open`** (`git-dir` + `common-dir`) for linked worktrees~~ — done in
   `gitana:repo@0.3.0`. `WorktreeFileStore` is now built over two `LocalFileStore`s (each an
   `Arc`, so a single-directory repository shares one store — one temp counter, one lock set)
   and is target-agnostic; only its cap-std `Dir` constructor stays native-only. The new
   `open-worktree(git-dir, common-dir)` export routes per-worktree paths (`HEAD`, in-progress
   state) to `git-dir` and shared paths (objects, refs, `packed-refs`, `config`) to `common-dir`;
   `open`/`init` build the same store over a single descriptor (`WorktreeFileStore::single`). The
   host e2e proves the split byte-for-byte in both hash formats (`tests/worktree.rs`).
3. **Worktree capability threading** — the ~58 ambient `std::fs` sites in `gitana-worktree`;
   after that, `porcelain::commit`/`merge`/`status` become exportable. WASI symlink/exec-bit
   limits need validation there.
4. **`wasi:http` transport trait** for `gitana-remote` so fetch/clone/push work in-component.
5. ~~**Retire `StdBackend`/`from_root`**~~ — done: `wasm-object-db` takes its preopen as a
   descriptor (`preopens#get-directories` → `from_descriptor`); the descriptor backend is the
   only wasm backend.
6. **WASI 0.3 revisit** — native async exports delete `block_on`; blocked on the Rust `wasip3`
   target maturing past Tier 3.
7. **Host embedding as a product crate** — `gitana-repo-host` is currently a test harness; a
   consumer-facing embedding API (and validation on a non-wasmtime runtime, e.g. jco) is future
   work.
