# Trust & Signing — Post-v1 Follow-ups

The 8-step trust & signing subsystem (`secure-git-trust-signing.md`) is complete and `require` is
production-ready. The items here are **additive** and were deliberately left out of v1 — none weakens
the current boundary, and each is independent. This doc scopes each so it can be picked up without
re-deriving the design.

## Status

| Item | Status | Effort |
|---|---|---|
| Signed `push --signed --delete` | ✅ done (`19c056f`) | — |
| `trust sync` audit event (`TrustRootAdopted`) | ✅ done (`6e3d909`) | — |
| One-time-nonce replay cache | ✅ done (`91b05df6`) | — |
| **Persisted require-time baseline** | **⏭ next up** | medium |
| OpenPGP signatures | pending | large (new dependency) |

Each remaining item is gated by the project's usual flow: its own worktree/branch, Codex-clean before
merge, and the `gta`/`gta-mcp` surface-parity lock where a CLI surface changes. See **Completed** at the
bottom for what the three done items shipped.

## Outstanding

### Persisted require-time baseline

**⏭ Next up.**

- **Goal.** An explicit, stored grandfather set captured when policy moves to `require`, consulted by
  object-signature enforcement instead of re-deriving it live on every push.
- **Why deferred.** v1 uses `protected_baseline` (`gitana-git-http/src/enforce.rs`): a live walk from
  the *current* protected-ref tips on every push, grandfathering everything reachable from them. The 8d
  decision kept it deliberately — it is correct and needs no stored state — but it is O(history) per push
  and its boundary shifts as tips move. This item makes the cutover explicit and enforcement
  incremental. Only worth doing once histories are large enough for the walk to matter.
- **Approach.** At the `require` cutover, snapshot the cutover state and store it; `verify_protected_tip`
  (also in `enforce.rs`) then treats that stored set as the grandfather boundary instead of calling
  `protected_baseline`. Everything a protected ref *newly* introduces past the snapshot must be signed,
  exactly as today — only the boundary's source changes.
- **Touch points.**
  - `gitana-git-http/src/enforce.rs` — `protected_baseline` / `verify_protected_tip` read the stored
    baseline (falling back to the live walk when absent).
  - `gitana-porcelain/src/trust.rs` — `trust_set_policy` writes the baseline when moving to `require`
    (or a new `trust_baseline` composite for an explicit command).
  - Storage — a new ref plus the object it names (see below).
  - `gta-core` / both CLI surfaces if an explicit `gta trust baseline` command is added (surface-parity).
- **Open decisions — with recommended defaults (still confirm before building):**
  - *Capture timing:* **auto in `trust_set_policy` when the target is `require`** (the natural cutover),
    with an explicit `gta trust baseline` command as an optional later nicety for re-baselining. The 8d
    slice deferred the explicit command; do not add it unless a need appears.
  - *Storage shape:* a **`refs/gitana/baseline` ref naming one object that encodes the whole cutover
    state** — a ref can name only a single object, so the multiple protected tips (and/or the
    grandfathered id-set) must be packed into it. Two representable encodings:
    1. **A synthetic commit whose parents are all the protected tips at cutover** (git-native; `verify_
       protected_tip` walks from that commit's parents as a fixed boundary — stable, but still a walk).
    2. **A manifest blob listing the grandfathered object ids** (or just the cutover tips) — O(1)
       membership (the real incrementality win), at the cost of a new, versioned serialisation and a
       larger object for big histories.
    Recommend starting with **(1)** — it reuses the existing tip-walk logic verbatim and needs no new
    format; move to **(2)** only if profiling shows the walk is the bottleneck. Do **not** try to point
    the ref straight at "the tips" — that is not representable for more than one tip.
  - *Absent baseline:* **fall back to the live `protected_baseline` walk** — so a repo that enabled
    `require` before this landed keeps working unchanged, and the persisted baseline is a pure
    optimisation, never a correctness dependency.
  - *Under `off`/`warn`:* only write the baseline when actually moving to `require`; lowering the policy
    later can leave it (harmless, unused) or clear it — clearing is tidier.
- **Correctness note.** The only objects a baseline grandfathers are *unsigned* ones (signed
  commits/tags pass `verify_commit`/`verify_tag` regardless of the baseline). So the persisted set is
  exactly "the unsigned history that existed at cutover", fixed — which is what an operator expects when
  they flip to `require`. Because it never grandfathers anything a signature check wouldn't already
  clear, an absent or stale baseline can only ever be *stricter* (more re-verification), never a bypass —
  which is why the live-walk fallback is safe.
- **Effort.** Medium. Tests: a `verify_protected_tip` test that a stored baseline grandfathers the
  cutover set while a post-cutover unsigned commit is rejected; a `trust_set_policy` test that the
  baseline ref is written on the `require` cutover.

### OpenPGP signatures

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
- **Open decisions.** Which library (`sequoia-openpgp` vs the pure-Rust `pgp` crate — the choice has real
  weight for a clean-room, unsafe-forbidding workspace); verify-only first vs also sign; how to represent
  OpenPGP keys in `trust.json` (armored public key vs fingerprint + keyring). Do not hand-roll crypto — a
  vetted lib owns parsing/verification.
- **Effort.** Large (new dependency + key model). Best split verify-only first (a new crate + the
  dispatch + fixtures), then signing as a separate slice.

## Completed

- **Signed `push --signed --delete`** (`19c056f`). `push_signed` gained a `delete` target →
  `delete_signed` sends a signed delete certificate (`<old> <zero> <ref>`); `build_cert` generalised to
  optional `old`/`new`; the CLI routes `--signed --delete` through `push_signed`. Two orthogonal
  authorization axes were documented: signing authorises *who* deletes (trust), the host's delete-refs
  grant (`force`) authorises deletes *at all* — a signed delete still needs the host to permit deletes,
  like stock git's `receive.denyDeletes`. Porcelain round-trip + `enforce.rs` accept + `receive_pack`
  wire-apply tests.
- **`trust sync` audit event** (`6e3d909`). Added `AuditEvent::TrustRootAdopted { anchor }`;
  `TrustSyncOutcome::Updated` carries the chain's bootstrap `anchor`; the `gta-core` sync handler prints
  the event to stderr on an adoption or fast-forward — completing the client-side audit vocabulary from
  step 7b.
- **One-time-nonce replay cache** (`91b05df6`). A host-supplied `NonceLedger` trait (with a
  `NoReplayCheck` no-op default) threaded through `verify_push_with_ledger` and `ReceiveOptions`; after a
  certificate verifies, its nonce is recorded and a still-fresh replay is a certificate failure (rejected
  under `require`, warned under `warn`). The pure core stays stateless — the ledger is the host's state;
  `verify_push` delegates with `NoReplayCheck`, so its call sites are unchanged. The seam only — no
  production ledger yet (no server binary), matching the step-7 "typed events, no sink" decision.
