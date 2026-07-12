# HTTP Authentication & Git Credential Flows

## Context

`TODO.md` (Remote & Protocol Parity): *"Add HTTP authentication hooks compatible with ordinary Git
credential flows."* Today gitana's Smart HTTP remote is **unauthenticated only**. `Origin::parse`
accepts `http(s)://…` and the transport sends every request with **no `Authorization` header**; a
`401` is turned into a `bail!` inside the transport impl. So `clone`/`fetch`/`pull`/`push` against
any authenticated host (GitHub, a private http-backend behind Basic auth) fails.

This is greenfield: there is **zero** credential / auth / netrc / askpass code in the repo. The goal
is to authenticate **the way git does**, so real platform credential helpers work unchanged — not to
wrap `git`.

**No code yet.** This document lays out the seam, git's credential model, a capability-based design,
the layering that makes it fit the existing transport shape, the slice plan, and the decisions Scott
signed off on (recorded at the end).

## The seam today

**The transport trait** — `gitana-remote/src/http_transport.rs`:

```rust
pub trait HttpTransport {
  fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>>>;
  fn post(&self, url: &str, content_type: &str, body: Vec<u8>) -> impl Future<Output = Result<Vec<u8>>>;
}
```

Deliberately tiny: returns the **whole** body as `Vec<u8>`, **no status, no headers, no auth input**.
Each impl turns non-2xx into an error internally (`reqwest_transport.rs:31,48`; `wasi_http_transport.rs:92`).
That last property is the crux: **a 401 is invisible above the transport** — the retry loop cannot
see it. This is the primary thing to change.

**Implementors (4):**

| Impl | Where | Notes |
|---|---|---|
| `ReqwestTransport` | `gitana-remote/src/reqwest_transport.rs` | native default (`reqwest-transport` feature); already reads `status` internally, just doesn't surface it |
| `WasiHttpTransport` | `gitana-repo-component/src/wasi_http_transport.rs` | in-guest `wasi:http`; builds `Fields` at `:47` (content-type only) — an `authorization` field slots in here |
| `CapturingTransport` / `ServerTransport` | test doubles in `gitana-porcelain` | must move with the trait |

**Where requests are actually issued** — and the layering wrinkle that shapes the whole design:

| Request | Issued at | Function |
|---|---|---|
| `GET /info/refs` (advertisement) | **CLI level**, before porcelain | `fetch_advertisement` (`clone.rs:45`, `fetch.rs`, `push.rs`, …) |
| `POST git-upload-pack` (fetch) | **inside porcelain** | `post_upload_pack` (`remote.rs:236`) |
| `POST git-receive-pack` (push) | **inside porcelain** | `send_receive_pack` (`remote.rs:1159`) |

So the auth flow straddles two layers: the advertisement GET is a frontend call, the pack POST is a
porcelain call. **A single "wrap the protocol call" retry loop cannot cover both.** The natural place
to put the 401 flow is therefore *below* both — in the transport itself, as a **decorator** that both
call sites go through transparently.

**`Origin`** — `gitana-remote/src/lib.rs:49` — `struct Origin { pub url: String }`. `parse()` trims a
trailing `/`, requires http(s), and **does not parse or strip userinfo**: `https://user:pass@host` is
stored verbatim and handed to the transport. Needs to split userinfo out (into a credential hint) and
expose host/path so credentials can key on the URL.

**Config plumbing (already available):** `Repository::effective_config()` returns the merged
system<global<local<`-c` view; the CLI installs it. `remote.*` already reads through it. A
`credential.*` reader slots in the same way. **No `[credential]` reader exists yet.** Note:
`url.*.insteadOf` is enumerable in config but the **transport path does not apply it** — `Origin` is
built from the raw URL (relevant to slice 4).

## Git's credential model (what "git-faithful" means here)

- **Flow:** send unauth → on **401** with `WWW-Authenticate: Basic` → resolve a credential → retry
  **once** with `Authorization: Basic base64(user:pass)` → on success `credential approve` (persist),
  on repeat 401 `credential reject` (erase). Bearer/Negotiate/NTLM are curl-handled in real git;
  **we start with Basic.**
- **Resolution order:** (1) URL userinfo `https://user:pass@host`; (2) `credential.<url>.username` /
  `credential.username` (a username *hint*, not a full credential); (3) **credential helpers**
  (`credential.helper`, multi-valued, plus per-URL `credential.<url>.helper`) — external programs
  over the *helper protocol*; built-ins `store`/`cache`/`osxkeychain`/`manager`/`libsecret`;
  (4) **prompt** via `GIT_ASKPASS` → `core.askPass` → `SSH_ASKPASS` → terminal
  (`GIT_TERMINAL_PROMPT=0` disables). `.netrc` is a contrib helper, not native to git's http path —
  out of scope.
- **Helper protocol (a stable contract, not "wrapping git"):** helper `foo` → program
  `git-credential-foo`; a `!`-prefixed value is a shell command, an absolute path runs directly.
  Actions `get`/`store`/`erase`; `key=value` on stdin (blank line ends), keys back on stdout. Input
  keys `protocol`/`host`/`path` (path only when `credential.useHttpPath=true`)/`username`.
  Implementing the protocol makes real platform helpers work unchanged.
- **Config keys:** `credential.helper`, `credential.<url>.helper`, `credential.username`,
  `credential.<url>.username`, `credential.useHttpPath`, `core.askPass`; env `GIT_ASKPASS`,
  `GIT_TERMINAL_PROMPT`; `url.<base>.insteadOf`/`pushInsteadOf`; `http.extraHeader` (multi-valued).

## Design

### A `CredentialProvider` capability (mirrors `Identity` / `Signer`)

The engine must not shell out, read ambient files, or prompt — exactly as it does not for identity or
signing. Add a capability trait the **CLI adapter** implements over git's credential machinery, and
that headless callers can decline cleanly. It lives in `gitana-remote` (the crate that owns "pair the
codec with an HTTP client"), alongside the transport:

```rust
/// Resolves and records HTTP credentials the git way. The engine holds this capability rather than
/// reading netrc / invoking helpers / prompting itself; the CLI adapter implements it, a headless
/// caller supplies a no-op (auth simply unavailable), and the wasm host grants it over WIT.
pub trait CredentialProvider {
  /// Resolve a credential for `request` (protocol/host/path/username hint). `Ok(None)` = none
  /// available (anonymous / no tty / declined) — the caller proceeds unauthenticated and lets the
  /// server's 401 stand.
  async fn fill(&self, request: &CredentialRequest) -> Result<Option<Credential>>;
  /// The credential worked — persist it (`git credential approve`). Best-effort; never fatal.
  async fn approve(&self, cred: &Credential) -> Result<()>;
  /// The credential was rejected — erase it (`git credential reject`). Best-effort; never fatal.
  async fn reject(&self, cred: &Credential) -> Result<()>;
}

pub struct CredentialRequest { pub protocol: String, pub host: String, pub path: Option<String>, pub username: Option<String> }
pub struct Credential { pub username: String, pub password: String }
```

`async fn` in trait (edition 2024, no `async_trait`, per conventions). Methods are async so the CLI
adapter can spawn a helper / askpass subprocess through `tokio::process` without blocking the runtime
— the same discipline `Signer` uses.

### The layering: two traits, and an authenticating decorator

The trait must surface **status** and accept a **request `Authorization` header** for the loop to work
— but I do **not** want to push that width up into porcelain, which is happy consuming bytes. So split
the seam in two:

1. **Raw client (widened seam).** Rename the byte-level responsibility to an `HttpClient` trait that
   returns status + body and accepts request headers:

   ```rust
   pub struct HttpResponse { pub status: u16, pub body: Vec<u8> }
   pub trait HttpClient {
     async fn get(&self, url: &str, headers: &[(String, String)]) -> Result<HttpResponse>;
     async fn post(&self, url: &str, content_type: &str, body: Vec<u8>, headers: &[(String, String)]) -> Result<HttpResponse>;
   }
   ```

   `ReqwestTransport` and `WasiHttpTransport` become `HttpClient` impls: they **stop** turning non-2xx
   into an error (they return the status) and **stop** turning "no auth" into a fixed request — they
   forward the caller's headers. They gain no credential knowledge whatsoever. (A transport error —
   DNS, TLS, connection reset — is still an `Err`; only HTTP status stops being an error.)

2. **Body-returning transport (unchanged consumer shape).** Porcelain keeps consuming a trait with the
   *exact* signature it has today — `get(url) -> Vec<u8>`, `post(url, ct, body) -> Vec<u8>` — so
   `fetch`/`clone`/`push`/`fetch_advertisement` **do not change their control flow at all**. Two
   implementors of it:

   - `AuthTransport<C: HttpClient, P: CredentialProvider>` — holds the raw client + the provider.
     Each `get`/`post` runs the git-faithful loop **once**: issue unauth → if `200`, done → if `401`
     with a Basic challenge, `fill` a credential, set `Authorization: Basic …`, retry once → on `2xx`
     `approve` and return the body → on repeat `401` `reject` and bail → any other non-2xx bails (as
     today). It parses `protocol`/`host`/`path` from the request URL for the `CredentialRequest`, and
     caches the filled credential across the get→post sequence of one operation so a single
     clone/fetch/push authenticates once, not per request.
   - `Unauthenticated<C: HttpClient>` — forwards to the client with no headers and restores today's
     "non-2xx → bail" behaviour. This is what the test doubles and anonymous callers use, and it keeps
     every existing unauth flow byte-for-byte unchanged.

This puts the entire git-faithful flow in **one place**, below both the CLI advertisement GET and the
porcelain pack POST, with auth-agnostic transports beneath it and an untouched porcelain above it.

```
                 gita-core (clone/fetch/push)          resolves the CredentialProvider, builds:
                        │
                        ▼
   AuthTransport<ReqwestTransport, CliCredentialProvider>   ← 401→fill→retry→approve/reject lives here
   (impl HttpTransport, body-returning — porcelain sees only this)
                        │  .get(url,[auth]) / .post(...)
                        ▼
   ReqwestTransport (impl HttpClient — status+headers, no auth knowledge)
```

### The CLI adapter: `CliCredentialProvider`

Lives in `gta-core` next to `CliIdentity` / `CliSigner`, resolved at the transport-construction sites
(`clone.rs:44`, `fetch.rs:45`, `push.rs:67`, `pull.rs`, `trust.rs:83`). It reads `effective_config`
and implements git's resolution order:

- **`fill`:** URL userinfo (from `Origin`) → `credential.<url>.username`/`credential.username` hint →
  **helpers** (slice 2) → **prompt** (askpass chain → terminal, honouring `GIT_TERMINAL_PROMPT=0` and
  no-tty by returning `Ok(None)`). Slice 1 does userinfo → config hint → prompt; helpers land in
  slice 2.
- **`approve`/`reject`:** no-ops until helpers exist (slice 2), where they invoke `store`/`erase`.

Headless/MCP paths get a `NoCredentials` provider (`fill` → `Ok(None)`), so an authenticated remote
fails with the server's own 401 message rather than hanging on a prompt.

### wasm: a host-granted credential capability (Scott: do it in this initiative → slice 3)

This design maps into the `wasm32-wasip2` component cleanly, and — verified against the component,
host, and `block_on` code — the two places a wasm design usually breaks (the sync invariant and the
capability shape) land *better* for credentials than they do for the network.

**The transport swap is localized.** `ops/remote.rs` builds `let transport = WasiHttpTransport;` and
hands `&transport` to porcelain's `fetch`/`clone`/`push` (each `&impl HttpTransport`) in all three
ops. Under the new layering the constructor becomes
`AuthTransport<WasiHttpTransport, WasiCredentialProvider>` (or `Unauthenticated<WasiHttpTransport>` in
slice 1) and **porcelain's signatures do not change**. `WasiHttpTransport` becoming an `HttpClient` is
a small edit: it already reads `status` (`wasi_http_transport.rs:90`, currently only to bail on
non-2xx) — it now *returns* the status — and it already builds `Fields` for content-type (`:47`),
where the passed-in `Authorization` header slots in. So the component diff is two small impls plus a
one-line constructor swap in three ops.

**The `block_on` sync invariant holds — trivially.** The component drives the exported async fn with a
noop waker and **traps** on any `Poll::Pending` (`block_on.rs:28`); its doc comment names "a future
`wasi:http` transport" as the hazard, which `WasiHttpTransport` dodges by blocking *inline* on the
pollable and returning an already-`Ready` future. A credentials **`func` import is a plain synchronous
component-model call** — no pollable, no stream — so `WasiCredentialProvider::fill` is an `async fn`
that makes a direct import call and returns `Ready` immediately, without even needing an inline
`.block()`. The host may implement the import as `async` (the host bindgen is
`imports: { default: async }`) and await a prompt/helper for as long as it likes; from the guest's
single-threaded view the call simply returns when the host resolves it. The invariant is preserved by
construction.

**The capability shape matches the `wasi:http` precedent — which is the correct one.** The component
has two capability flavors: *descriptors passed as function arguments* (`git-dir`/`common-dir`/
`work-dir` — per-call, unforgeable handles) and *interface imports granted at instantiation*
(`wasi:http/outgoing-handler` — ambient to the instance, but only if the host links it; the store
otherwise grants nothing). Credentials is inherently a **callback** capability: the value isn't known
until a 401 arrives, and resolving it may consult a helper/prompt on the host — so it *must* be a
host-answered import, exactly the `wasi:http` model. Passing credentials as descriptors would be the
inconsistent choice; the import is the faithful one, and the "no ambient authority" invariant is
preserved verbatim — the component obtains only a credential the host chose to grant.

Add an import to `world repo`:

```wit
// gitana:repo/credentials — the host answers credential requests for the remote porcelain. As built,
// the record and callbacks mirror the post-slice-2 native `CredentialProvider`: the request carries
// `wwwauth` (the 401 challenge) and `approve`/`reject` take the request (a helper keys its store on it).
interface credentials {
  record credential-request {
    protocol: string, host: string, path: option<string>, username: option<string>, wwwauth: list<string>,
  }
  record credential { username: string, password: string }
  fill: func(request: credential-request) -> option<credential>;
  approve: func(request: credential-request, cred: credential);
  reject: func(request: credential-request, cred: credential);
}
world repo {
  import wasi:filesystem/types@0.2.12;
  import wasi:http/outgoing-handler@0.2.12;
  import credentials;              // ← new; our own package, so no host bindgen `with:` remap needed
  export porcelain;
}
```

The package version bumps `0.7.0` → `0.8.0` (a new import). The host (`gitana-repo-host`) implements the
generated `credentials::Host` trait on `State` and the bindgen adds it to the linker — **manual wiring,
unlike `wasi:http`**, which gets wasmtime's prebuilt `add_only_http_to_linker_async`. That is the one
extra cost, and it is modest: no new crate dependency, no `with:` remap (the interface is in our own
`gitana:repo` package). Rather than a canned constant, `State` holds a pluggable
`Option<Box<dyn HostCredentialProvider>>` — the embedder's real seam — and the harness ships a
`StoreFileCredentials` default (git's `credential-store` file format) so the end-to-end Basic-auth test
proves a genuine `fill` → `approve` (persist) → re-`fill` round-trip, not a hardcoded value.

**Conventions.** `CredentialProvider` is a native `async fn` trait (edition 2024, no `async_trait`),
like `Identity`/`Signer`/`HttpTransport`. It, `HttpClient`, `AuthTransport`, and `Unauthenticated`
live in `gitana-remote` and must **not** sit behind the `reqwest-transport` feature, so the component
compiles them; being generic, they pull no `reqwest` into wasm. `WasiCredentialProvider` is one type
in its own file, mirroring `WasiHttpTransport` / `HostIdentity`.

This slots *after* the native machinery (slice 1) fixes the `CredentialProvider` trait shape — you
can't model the host import cleanly until the capability's surface is settled. Slice 1 keeps the
component byte-for-byte unchanged by wrapping `Unauthenticated<WasiHttpTransport>`, so wasm stays green
with no half-authenticated intermediate state.

**One dependency check for slice 1:** Basic auth base64-encodes `user:pass` inside `AuthTransport`
(in `gitana-remote`), so the workspace needs a base64 that builds on `wasm32-wasip2`. Confirm an
existing wasm-clean crate is reused (trust/protocol code likely already pulls one) rather than adding
a new dependency; if none exists it is a one-line addition, not a design issue.

## Slice plan

Each slice: its own worktree+branch, workspace green **including `wasm32-wasip2 --all-targets`**,
`cargo fmt`, codex-review clean, Claude squash-merges after Scott approves.

- **Slice 1 — Basic auth core (native).** Split `HttpClient` (status + request headers) from a
  body-returning `HttpTransport`; move `ReqwestTransport`/`WasiHttpTransport`/test-doubles onto
  `HttpClient`; add `AuthTransport` + `Unauthenticated`. Add the `CredentialProvider` capability +
  the 401→fill→retry→approve/reject loop. `Origin` parses/strips userinfo + exposes host/path.
  `CliCredentialProvider` resolving userinfo → `credential.username` config → askpass/terminal prompt
  (honour `GIT_TERMINAL_PROMPT`, decline cleanly with no tty). wasm builds with `Unauthenticated`
  (unchanged behaviour) until slice 3. Oracle: a Basic-auth-gated axum server in
  `crates/cli/gta/tests/support/mod.rs`.
- **Slice 2 — Credential helper protocol (native). ✅ Done.** `CliCredentialProvider` now drives
  external helpers over git's protocol: `fill` runs the resolved `get` chain (feeding forward the known
  username and the `401`'s `wwwauth[]`, stopping when both fields are known, aborting on a helper's
  `quit`) *before* prompting; `approve`/`reject` run every helper's `store`/`erase`. Implemented in a
  new `gta-core` `credential_helper` module — `Helper` (the three value forms `!shell` / absolute path
  / `git credential-<name>`, run via `sh -c` from `cwd`, mirroring git's `use_shell`) and `resolve`
  (which helpers apply, plus `username` and `useHttpPath`). **Full `credential.<url>` matching** is
  git-faithful (`urlmatch.c` `match_urls` for a full URL — scheme-exact, optional-user-exact, host with
  `*` wildcarding a single label, port after default-port stripping, path prefix at a `/` boundary —
  and `credential.c` `credential_match` for a scheme-less partial; **credentials use `select_all`, so
  there is no specificity ranking — every matching entry applies in read order, single keys last-wins,
  `helper` accumulates with `helper=` resetting**). Wire format verified empirically against git 2.50:
  git sends decomposed `protocol`/`host`/`path`(only under `useHttpPath`)/`username`/`password`
  + `wwwauth[]` to helpers and does **not** send `url` (that attribute is a *caller→git-credential*
  input convenience, never a git→helper output), so gitana threads `wwwauth[]` (surfacing it end to end
  by making `HttpResponse.www_authenticate` a `Vec<String>` and carrying it on the fill
  `CredentialRequest`) and omits `url`. No gitana-native store — external `git-credential-*` only.
  Oracle: a self-contained file-backed `git-credential`-style helper in `git_http_auth.rs` exercising
  the `get` / `approve→store` / `reject→erase` round-trip, plus regression that the slice-1 userinfo /
  askpass / prompt flows are unchanged when no helper is configured. Hardened over ~16 codex rounds
  against git 2.50 source + probes: the full `get`/`store`/`erase`/`quit`/`url=` state machine (a `url=`
  clears the credential *and* the helper chain, so no later helper runs and no `store`/`erase` fires; a
  malformed `quit`/`url=` aborts as git dies; a `get` helper's output is consumed even on a non-zero
  exit); `credential.<url>` URL normalisation reproducing git's `url_normalize`/`credential_format`
  asymmetry (unreserved-only decode on the pattern vs full `credential_format` re-encode on the request,
  so `a%2Fb`↔`a/b`, `a%00b`→`%2500`, a literal `:` pattern *not* matching, wildcard/`*` host, one-dot
  FQDN strip, IPv6 brackets, malformed `%XX`/`%00` → exact partial fallback, numeric-port default
  stripping); byte-exact helper `path=` (raw `0xFF` survives, `%00` kept literal so no `key=value`
  truncation); `approve`/`reject` re-running the *same* helper chain `fill` settled (keyed on the
  fill-time username hint, suppressed entirely after a `url=` reset), matching git's once-per-operation
  `credential_apply_config`; the `401` re-auth being a single fill+retry then give-up (git's
  `HTTP_NOAUTH`, not a refill loop); and userinfo redacted from all malformed-`url=` error messages.
  **Deliberately deferred (not needed for osxkeychain/store/manager/cache):** `capability[]`/`authtype`/
  `state[]` negotiation, `password_expiry_utc`/`oauth_refresh_token`, threading a helper's refined
  `protocol`/`host`/`path` forward, and `.`/`..` path dot-segment resolution (repo paths lack them). One
  intentional divergence: `credential.<url>` matching keys on the URL-userinfo username only, so a
  username *learned* from `credential.username`/a helper does not retroactively enable a
  username-qualified `credential.<user>@host` section (git's single-pass config walk can).
- **Slice 3 — wasm host-import credential capability. ✅ DONE.** WIT `credentials` import (package
  `0.8.0`, record mirroring the post-slice-2 native trait — `wwwauth` on the request, the request on
  `approve`/`reject`) + `WasiCredentialProvider` (guest, own file, swapping the three
  `Unauthenticated::new` for `AuthTransport::with_userinfo`) + `gitana-repo-host` `credentials::Host` on
  `State` behind a pluggable `HostCredentialProvider` (with a `StoreFileCredentials` default) + linker
  wiring. Host-harness end-to-end: a `401`-gated axum server, a store-file-backed `fetch`/`clone` that
  authenticates in both hash formats, and a no-credential run where the `401` stands. No error/quit
  channel over WIT (a host declines by returning `none` from `fill`).
- **Slice 4 — URL rewriting, redirects + extra headers.** Apply `url.*.insteadOf`/`pushInsteadOf` in
  the transport path (currently display-only), `http.extraHeader`, per-URL `credential.*` matching,
  possibly Bearer tokens, and **cross-host redirect following for auth** (`http.followRedirects`). The
  last belongs here, not slice 1: it is URL-rewriting-shaped (remember the redirect target and
  re-address subsequent requests), and doing it *safely* (never sending a credential across an origin
  it was not resolved for) is a self-contained problem best designed with its own cross-origin test
  matrix. Slice 1 deliberately relies on reqwest's safe default instead — a cross-host-redirected
  repository fails to authenticate rather than leaking a credential.

## Verification

Add `serve_gitana_basic_auth(git_dir, hash, user, pass)` to `crates/cli/gta/tests/support/mod.rs`
(mirroring `serve_gitana_with_reflog`) — an axum wrapper that returns `401 WWW-Authenticate: Basic
realm=…` when the `Authorization` header is absent/wrong. Assert `gta` acquires and sends credentials
from (a) URL userinfo, (b) `credential.username` config + scripted askpass via `GIT_ASKPASS`, and
that a wrong credential yields git's reject/re-prompt. Cross-check against stock `git` as client and
server where practical (extend the `gta_against_git_http_backend` / `real_git_interop` harnesses with
the Basic-auth wrapper). Regression: existing unauthenticated flows unchanged (guaranteed structurally
by `Unauthenticated`, plus the existing suite staying green).

## Decisions (Scott, 2026-07-11)

1. **Short HLD first** — this document, before code. ✅
2. **Native machinery + helper protocol (both).** Implement the helper protocol (slice 2) so real
   platform helpers work; native prompt + Basic in slice 1. Do **not** reimplement platform keychains.
3. **Interactive prompting in slice 1** — URL userinfo → config → askpass/terminal, declining cleanly
   with no tty (MCP/headless).
4. **wasm auth: host-import capability now** — as slice 3 in this initiative (not deferred), a
   host-granted credential import over WIT.
5. **Basic only first**, Bearer/others later (slice 4).

6. **Trait naming (resolved — proceeding).** The split names the raw status+headers seam `HttpClient`
   and keeps the body-returning consumer trait `HttpTransport`, so porcelain's call sites are untouched
   by name. Trivially reversible if a later reviewer prefers naming the raw trait `HttpTransport` and
   the decorator `PackTransport`/`AuthedTransport`.
