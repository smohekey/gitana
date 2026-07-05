# wasi:http Transport Trait

## Context

Roadmap item 4 from `docs/hlds/wasi-component-porcelain.md`. The `gitana:repo` wasm component now
exports the repo-level plumbing set **and** the working-tree porcelain (`status`/`add`/`checkout`/
`commit`) over passed-in `wasi:filesystem` descriptors — no preopens, no ambient authority. The last
capability the porcelain reaches for that the component cannot yet grant is **the network**: `clone`,
`fetch`, `pull`, and `push` talk to a Smart HTTP remote.

The single thing that blocks them: **`gitana-remote` reaches the network through a hard `reqwest`
dependency**. Everything above that seam is already transport-shaped — the wire codec
(`gitana-git-http`) is transport-agnostic, and `gitana-porcelain`'s production code is otherwise
capability-clean (its only `std::fs`/`cap-std` uses are under `#[cfg(test)]`). `reqwest` is
*literally the sole reason* `gitana-porcelain` is kept out of the wasip2 reactor today (documented in
`gitana-repo-component/Cargo.toml`).

This document is the design pass the roadmap called for. **No code yet** — it lays out the seam, the
one genuinely hard problem (the async story), the options for solving it with a recommendation, the
slice plan, and the decisions that need Scott's sign-off before implementation.

## The seam is already tiny

`gitana-remote/src/http.rs` has exactly two functions that touch `reqwest`:

```rust
pub async fn http_get(url: &str) -> Result<Vec<u8>>;
pub async fn http_post(url: &str, content_type: &str, body: Vec<u8>) -> Result<Vec<u8>>;
```

Every network path funnels through them:

| Caller | via |
|---|---|
| `fetch_advertisement(origin, service)` (clone/fetch/pull/push, `gta-core`) | `http_get` |
| `fetch_pack(origin, repo, wants, haves)` → `download` (porcelain `fetch`/`clone`) | `http_post` |
| porcelain `push` (push + delete) | `http_post` directly |

So the entire transport surface is **GET → bytes** and **POST(content-type, body) → bytes**. Smart
HTTP v0 as gitana speaks it is strictly request → *complete* response (the pack response is parsed
whole; no sideband interleave is consumed mid-stream), which is what makes the async story tractable
below.

## The trait

Introduce a dependency-free trait in `gitana-remote` (it already owns "pair the codec with an HTTP
client"):

```rust
pub trait HttpTransport {
    async fn get(&self, url: &str) -> Result<Vec<u8>>;
    async fn post(&self, url: &str, content_type: &str, body: Vec<u8>) -> Result<Vec<u8>>;
}
```

`async fn` in trait (edition 2024, per conventions — no `async_trait`). The two free functions
`fetch_advertisement` / `fetch_pack` and the porcelain `fetch`/`clone`/`push` grow a
`transport: &impl HttpTransport` parameter; `http_get`/`http_post` become the body of the native
impl rather than free functions.

### Native impl — `reqwest` becomes optional

`ReqwestTransport` (the current `http_get`/`http_post`, verbatim) moves behind a **default** cargo
feature on `gitana-remote`, e.g. `reqwest-transport`. `reqwest` moves to
`optional = true`. `gta-core` keeps the default feature and passes a `ReqwestTransport`. When the
component builds `gitana-remote` (transitively via `gitana-porcelain`) it does so with
`default-features = false`, so **`reqwest` never enters the wasip2 reactor** — the whole point.

### Wasm impl — `WasiHttpTransport`

Lives in the component crate (the only crate with `wit_bindgen` glue and the wasi imports). The
component's world gains an import of `wasi:http/outgoing-handler@0.2.12` (WIT vendored into
`wit/deps/`, matching the 0.2.12 filesystem/io already there; wasmtime-wasi 46 provides the host
impl). `WasiHttpTransport::{get,post}` build an `outgoing-request`, call `handle`, and read the
response body to a `Vec<u8>`.

## The crux: the async story

This is the one hard part, and the porcelain doc names it explicitly:

> A poll-count bail-out turns any future violation of that invariant (e.g. a `wasi:http` transport)
> into a trap … — `wasi-component-porcelain.md`, "The Async Story"

Today the component drives synchronous WASI 0.2 exports with a **noop-waker `block_on`** that is
sound *only because nothing on the path ever awaits a `wasi:io` pollable* — every `.await` bottoms
out in something immediately `Ready` (the file store's inline `blocking()` helper, an uncontended
mutex, the synchronous descriptor reader). `wasi:http` is inherently pollable-driven: you get a
`future-incoming-response` and an `input-stream`, each with a `pollable` you must wait on. Naively
awaiting those inside an op future returns `Pending`, the noop-waker spins, and the bail-out counter
traps. Three ways through:

### Option A — synchronous blocking wasi:http helper (recommended)

Write `WasiHttpTransport::{get,post}` as `async fn`s whose **bodies are fully synchronous**: call
`outgoing-handler.handle`, then block on `wasi:io/poll.poll([future.subscribe()])` until the response
is ready, then read the body stream in a loop blocking on *its* pollable via `poll.poll`, and return
the assembled `Vec<u8>`. `poll.poll(list<pollable>)` is WASI 0.2's blocking wait primitive — it
parks the whole component until a pollable is ready, then returns.

The future never yields `Pending` to our executor — exactly like the file store's `blocking()`
helper returns `Ready` inline. **The entire `block_on` story is preserved unchanged**; the bail-out
never fires because the op future still completes in bounded polls. This is adequate precisely
because gitana's Smart HTTP is request → complete-response with no concurrency: one blocking request
at a time is the actual workload.

- Pro: no async runtime crate; preserves the proven `block_on`; uses **standard `wasi:http`**, so the
  component stays portable to any WASI host that provides it (jco, wasmtime) — aligned with roadmap
  item 7 (host embedding / non-wasmtime validation).
- Con: blocks the single component task for the request duration (a non-issue for a reactor doing one
  clone/fetch/push at a time); we hand-roll the blocking read loop instead of leaning on a runtime.

### Option B — host-provided custom transport import

Define a gitana-specific WIT import (`get`/`post` returning `list<u8>`) that the **host** implements
(over reqwest *or* wasmtime's own `wasi:http`). The guest sees a plain synchronous WIT call; the host
blocks on async I/O behind it. Also preserves `block_on` (guest never touches a pollable).

- Pro: simplest guest code; host picks the HTTP stack.
- Con: the component is **no longer self-contained for HTTP** — every embedder must implement a
  bespoke gitana import, which is *less* standard than `wasi:http` and works against the
  "portable to any WASI host" goal. Philosophically it is still capability-passing, but it invents a
  capability the ecosystem already standardized.

### Option C — a real pollable-aware executor

Replace the noop-waker `block_on` with a minimal reactor executor that, on `Pending`, gathers the
tasks' registered pollables and calls `poll.poll`, then re-polls. This is the "correct" general
answer and what WASI 0.3 obsoletes — but it is a meaningful rewrite of the load-bearing shim for zero
functional gain over Option A on this workload, and it needs the async-wasi plumbing (a `Reactor`
mapping wakers ↔ pollables) that Option A sidesteps entirely.

- Pro: general; would support concurrent/streamed requests if ever needed.
- Con: rewrites the proven shim now, for a capability (concurrency) the workload does not use; more
  code and more risk than A.

**Decision (Scott): Option A.** It uses the standard interface (unlike B), keeps the proven
`block_on` untouched (unlike C), needs no new runtime dependency, and matches the actual
request/response workload. B stays a fallback if `wasi:http` in-guest proves fiddly under wasmtime
46; C is really just "the WASI 0.3 migration" (roadmap item 6) arriving early and should be deferred
to it.

## Threading the trait through

Signatures that grow a `transport: &impl HttpTransport` (or `&T`) parameter:

- `gitana-remote`: `fetch_advertisement`, `fetch_pack`.
- `gitana-porcelain::remote`: `fetch`, `clone`, `push` (they call the above / `http_post`).
- `gta-core` commands `clone`/`fetch`/`pull`/`push`: construct `ReqwestTransport` and pass it.

`Origin` and the codec stay untouched. `gta-mcp` inherits via `gta-core`. No public data types change
shape — only added parameters.

## New component exports

Once `gitana-porcelain` builds for wasm (reqwest gated off) the component can pull it in and add
`clone` / `fetch` / `push` exports on the `repository` resource (or a sibling), each constructing a
`WasiHttpTransport` internally. `pull` composes `fetch` + `merge` in the adapter today; the component
can expose `fetch` and let a host compose, or add `pull` once `merge` lands as an export (still the
open worktree item). Exact WIT surface is a per-slice decision, not settled here.

## Slice plan

1. ~~**Trait + native impl, `reqwest` gated.**~~ **Done** (merged to main, `3739fe1`). Introduced
   `HttpTransport`, moved `http_get`/`http_post` into `ReqwestTransport` behind a default feature,
   threaded the parameter through `gitana-remote`/`gitana-porcelain`/`gta-core`. Pure refactor —
   native behaviour identical; `gitana-porcelain` now `cargo check`s for wasm32-wasip2 with
   `--no-default-features` (reqwest gone).
2. ~~**Vendor `wasi:http` WIT + `WasiHttpTransport` + `fetch` export, proven e2e.**~~ **Done** (this
   slice, `gitana:repo@0.5.0`). Vendored a trimmed `wasi:http@0.2.12` WIT (the `types` +
   `outgoing-handler` interfaces only — dropping the `imports`/`proxy` worlds that reference
   `wasi:random`/`wasi:cli`), remapped it onto the `wasip2` crate, and imported
   `wasi:http/outgoing-handler` into the world. `WasiHttpTransport` is the synchronous blocking Option
   A client (blocks inline on `wasi:io` pollables — `Pollable::block`, `InputStream::blocking_read`,
   `OutputStream::blocking_write_and_flush` in ≤4 KiB chunks — so `block_on` never sees `Pending`).
   The host gained `wasmtime-wasi-http` (`WasiHttpView` on `State`, `add_only_http_to_linker_async`).
   The `fetch` export runs `porcelain::fetch` over the transport; host e2e `tests/remote.rs` proves a
   real fetch from a loopback axum Smart-HTTP server in both hash formats. **Only `fetch`** landed —
   it proves the whole architecture end to end.
3. ~~**Component `clone`/`push` exports** + e2e~~ **Done** (`gitana:repo@0.6.0`). Both reuse
   `gitana-porcelain`'s composites unchanged over the in-guest `WasiHttpTransport`. To make
   `porcelain::clone` capability-clean, `Origin::save` moved off `std::fs`/`&Path` onto
   `&impl FileStore` (async; writes `config` through the store) — the last ambient-fs call on the
   remote path, mirroring the worktree-threading precedent; `clone` then drops its `git_dir: &Path`
   parameter and persists the origin through `repo`'s file store. `clone` is a **static func**
   (`git-dir`, `work-dir`, `url`): it negotiates the object format from the wire advertisement (there
   is no local config to detect one from yet), lays the git skeleton, runs the clone, and — because a
   clone populates directories rather than opening one — consumes both descriptors and returns unit
   (reopen with `open-worktree` to operate on the result). `push(url, force, delete)` is a method that
   reuses `porcelain::push` with `signed = false`; the pusher-identity resolver (only reached for a
   signed push) unconditionally errors, so the **unsigned** receive-pack POST is wired and signing
   stays out until the trust work. Host e2e `tests/remote.rs` gains a `git-receive-pack` route over
   gitana's own `receive_pack` handler and proves clone + push round-trips in both hash formats.

## Verification gate (per slice)

Per the established gate: `cargo build/test --workspace` · `cargo check --target wasm32-wasip2
--all-targets` for every wasm-capable crate **plus `gitana-remote`/`gitana-porcelain` with
`gitana-remote`'s default features off** · `cargo fmt --all -- --check` · `RUSTDOCFLAGS="-D warnings"
cargo doc --no-deps -p <touched>`. Codex `--base main` until clean. e2e: native gitana / stock git as
the oracle; the guest must match byte-for-byte in both hash formats.

## Open questions for Scott

1. ~~**Option A vs B** for the wasm transport~~ — **decided: Option A** (standard in-guest
   `wasi:http`, called synchronously). B stays the low-risk fallback if in-guest `wasi:http` proves
   fiddly under wasmtime 46.
2. ~~**Feature name / default** on `gitana-remote`~~ — settled: `reqwest-transport`, default on;
   `gitana-porcelain` mirrors it (`reqwest-transport = ["gitana-remote/reqwest-transport"]`). Both
   default off at the workspace root so members opt in (gta-core explicitly; the component omits it).
3. ~~**Scope of the first component export set**~~ — decided: `fetch` first (proves the transport,
   smaller surface); `clone`+`push` as a follow-up. `push`'s signing path stays unwired.
4. ~~Whether to pull **all of `gitana-porcelain`**~~ — pulled the whole crate in (reqwest gated off it
   builds for wasip2 cleanly, and its production code was already capability-clean), reusing
   `porcelain::fetch` unchanged rather than reimplementing it.
