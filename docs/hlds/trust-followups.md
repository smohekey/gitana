# Trust & Signing — Post-v1 Follow-ups

The 8-step trust & signing subsystem (`secure-git-trust-signing.md`) is complete and `require` is
production-ready. The items below are **additive** and were deliberately left out of v1 — none of them
weakens the current boundary, and each is independent. This doc scopes each one so it can be picked up
without re-deriving the design.

Suggested order (cheapest / highest-leverage first):

1. [Signed `--signed --delete`](#1-signed-push---signed---delete) — closes a real `require` gap; small.
2. [`trust sync` audit event](#5-trustrootadopted-audit-event-for-trust-sync) — completes the client
   audit vocabulary; small.
3. [One-time-nonce replay cache](#2-one-time-nonce-replay-cache) — security hardening; medium.
4. [Persisted require-time baseline](#4-persisted-require-time-baseline) — enforcement perf/stability;
   medium.
5. [OpenPGP signatures](#3-openpgp-signatures) — GPG interop; large, new dependency.

Each remains gated by the project's usual flow: its own worktree/branch, Codex-clean before merge, and
the `gta`/`gta-mcp` surface-parity lock where a CLI surface changes.

---

## 1. Signed `push --signed --delete`

**Also tracked as a checkbox in `TODO.md` (Signing And Integrity).**

- **Goal.** A signed deletion of a protected ref. Today `--signed --delete` sends an *unsigned* delete
  (the delete path returns before the certificate is built), so under `require` a protected-ref deletion
  cannot be authorised — receive-pack already refuses it (`enforce.rs::require_rejects_protected_deletion_without_cert`).
- **Why deferred.** Noted as a known gap when `gta push --signed` landed (step 6c); the delete branch
  predates `push_signed`.
- **Approach.** Fold the deletion into a `push_signed` certificate as one cert command `<old> <zero>
  <ref>` (git's delete form). Route a signed delete through `push_signed` instead of the early-return
  unsigned path. Nothing new is needed server-side — `verify_push` already verifies certificates for
  deletions.
- **Touch points.** `gitana-porcelain/src/remote.rs` (accept a delete command in `push_signed` /
  `prepare_branch_push`), `gta-core` `commands/push.rs` (dispatch `--signed --delete`).
- **Open decisions.** Minimal — mostly mechanical. Confirm the empty pack is acceptable for a
  delete-only signed push (it is: `receive_pack` handles an empty pack).
- **Effort.** Small–medium. Add a porcelain round-trip and extend the real-git e2e
  (`real_git_push_signed.rs`) with a signed delete.

## 2. One-time-nonce replay cache

- **Goal.** Reject a push certificate whose fresh, valid nonce has already been used — closing the
  replay-*within*-the-freshness-window gap (matrix row 6, ⚠️ by design).
- **Why deferred.** v1's nonce is a stateless HMAC (`timestamp || random || HMAC(secret, …||repo_id)`),
  so it needs no server state; replay-in-window is accepted and documented. Closing it requires
  short-lived server state.
- **Approach.** After `verify_cert` passes, consult and record the nonce in a TTL store (TTL = the
  freshness window). Keep the core pure: add a host-supplied capability (a small trait, e.g.
  `NonceLedger { async fn seen_and_record(&self, nonce: &str) -> bool }`) threaded through
  `TrustContext` (or a new param on `verify_push`). Single-instance → in-memory map; multi-instance →
  a shared cache (the HLD's "shared cache or bounded replay" note).
- **Touch points.** `gitana-git-http` (`TrustContext` / `verify_push` / `enforce.rs::verify_cert`), the
  embedding host (test harness + any future server) provides the ledger.
- **Open decisions.** The ledger trait shape; whether to key on the full nonce or its hash;
  single- vs multi-instance semantics. Must not make the core stateful — state lives in the host.
- **Effort.** Medium.

## 3. OpenPGP signatures

- **Goal.** Verify (and optionally produce) OpenPGP-signed commits, annotated tags, and push
  certificates alongside SSHSIG, for GPG interoperability.
- **Why deferred.** v1 is SSHSIG-only by explicit choice (dependency cost). Verification is purely
  additive — a second branch in the trust core selected by the armor marker.
- **Approach.** `gitana-object` already preserves the `gpgsig` header byte-exactly (and `gpgsig-sha256`
  for SHA-256 repos) via generic extra-headers, so the object layer likely needs no change. In
  `gitana-trust`, dispatch on the signature armor (`-----BEGIN PGP SIGNATURE-----` vs
  `-----BEGIN SSH SIGNATURE-----`) inside `verify_commit`/`verify_tag`, add an OpenPGP verify path beside
  `verify_sshsig`, and extend `TrustedKey`/`KeyId` (today OpenSSH-only) to carry an OpenPGP key. Client
  signing would add a GPG `Signer` (shelling to `gpg`, parallel to the `ssh-keygen` `CliSigner`).
- **Touch points.** `gitana-trust` (verify path, `TrustedKey`, `KeyId`, `TrustDocument` key parsing),
  `gitana-porcelain`/`gta-core` (a GPG signer, if producing), a vetted OpenPGP crate.
- **Open decisions.** Which library (`sequoia-openpgp` vs `pgp`); verify-only first vs also sign; how to
  represent OpenPGP keys in `trust.json` (armored public key vs fingerprint + keyring). Do not hand-roll
  crypto — a vetted lib owns parsing/verification.
- **Effort.** Large (new dependency + key model).

## 4. Persisted require-time baseline

- **Goal.** An explicit, stored grandfather set captured when policy moves to `require`, consulted by
  object-signature enforcement — instead of re-deriving it live on every push.
- **Why deferred.** v1 uses `protected_baseline` (a live walk from the *current* protected-ref tips at
  each push). That already grandfathers existing history correctly (the 8d decision kept it), but it is
  O(history) per push and shifts if tips move.
- **Approach.** At the require cutover (auto in `trust_set_policy`, or an explicit `gta trust baseline`),
  snapshot the objects reachable from the current protected tips into a stored artifact — e.g. a ref
  `refs/gitana/baseline` pointing at those tips, or a serialised id set. `verify_protected_tip` then uses
  the stored baseline instead of the live walk, making enforcement incremental and stable.
- **Touch points.** `gitana-git-http/enforce.rs` (`protected_baseline` reads the stored set), a writer at
  require-time (`gitana-porcelain` `trust_set_policy` or a new command), ref/blob storage.
- **Open decisions.** Storage shape (a ref to the tips vs a persisted id set); captured automatically at
  `set-policy require` vs an explicit command (8d deferred the explicit command); fallback to the live
  walk when absent.
- **Effort.** Medium.

## 5. `TrustRootAdopted` audit event for `trust sync`

- **Goal.** Emit a typed `AuditEvent` when `gta trust sync` adopts (bootstrap) or fast-forwards the local
  trust root, completing the client-side audit vocabulary started in 7b.
- **Why deferred.** Descoped from 7b: `trust sync` already prints its anchor prominently, and threading
  the anchor out to an event would have churned `TrustSyncOutcome` and its tests.
- **Approach.** Add `AuditEvent::TrustRootAdopted { anchor }` (and/or an updated/synced variant). Either
  extend `TrustSyncOutcome::Updated` with the anchor (`Some` on bootstrap adoption) or have `trust_sync`
  return the event; the `gta-core` sync handler `eprintln!`s it like the other trust ops.
- **Touch points.** `gitana-trust` (`AuditEvent` variant), `gitana-porcelain/src/trust.rs`
  (`TrustSyncOutcome` / `trust_sync`), `gta-core` `commands/trust.rs` (sync handler).
- **Open decisions.** Whether `Updated` gains an `anchor` field (churns ~5 sync tests) vs a returned
  event; whether to also audit fast-forward syncs (not just bootstrap adoption).
- **Effort.** Small.
