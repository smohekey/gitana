# Secure Git Trust And Signing Plan

## Context

Gitana already has a SHA-256-native Git implementation with object codecs, refs, working-tree support,
Smart HTTP fetch/push, and early push-certificate protocol machinery. The next step is a trust and
signing subsystem that makes repository writes tamper-evident and resistant to stolen write credentials.

This plan is intentionally stricter than a report-only rollout. Trust verification must be part of the
pre-receive security boundary from the first enforcing phase: protected refs move only after the trust
root, push certificate, and newly introduced signed objects have been verified.

## Security Contract

The implementation should preserve these invariants:

- A write token or session alone cannot update protected refs.
- Protected refs require a fresh push certificate signed by a trusted key when policy is `require`.
- New commits and annotated tags require trusted object signatures when policy is `require`.
- Trust-root updates are verified before `refs/gitana/trust` moves.
- An absent trust root means no trust enforcement; a malformed or unverifiable existing trust root fails
  closed for protected writes.
- Recovery is an explicit operator action with audit output, not a silent bypass.
- Verification never depends on catalog/cache state as the source of truth; cache entries are rebuilt
  from signed Git state.

Policies:

| Policy | Behavior |
|---|---|
| `off` | No trust enforcement. |
| `warn` | Verify and record failures, but do not reject writes. |
| `require` | Reject unsigned, untrusted, stale, malformed, or unverifiable protected writes. |

## Git-Native Formats

Use standard Git signing formats so stock Git can interoperate:

- Push authentication: `git push --signed` push certificates.
- SSH signatures: `SSHSIG` with namespace `git`.
- OpenPGP signatures for GPG interoperability, if the dependency cost is acceptable.
- Signed commits: `gpgsig` for SHA-1 repos if ever supported, and `gpgsig-sha256` for Gitana's
  SHA-256 repos.
- Signed tags: annotated tag objects with preserved signature payloads. Lightweight tags can exist, but
  they are not sufficient under `require` when tag authenticity matters.

Do not introduce custom cryptography. Gitana owns policy, storage, and verification orchestration; vetted
libraries own signature parsing and cryptographic verification.

## Trust Core Library

Add a pure trust crate, for example `gitana-trust`, with no server, catalog, CLI, or network dependency.

Responsibilities:

- Parse and preserve signed commit and annotated-tag payloads byte-for-byte.
- Compute the exact signature-stripped payload Git verifies.
- Verify SSHSIG and OpenPGP detached signatures against trusted public keys.
- Parse and canonicalize trusted public keys.
- Represent the trust document:

  ```rust
  TrustRoot {
      version,
      policy,
      keys,
      metadata,
  }
  ```

- Fold a trust-root chain from a given tip.
- Verify a candidate trust-root update from an old tip to a new tip without mutating refs.
- Verify commits, annotated tags, and push certificates against a folded root.

Important API shape:

```rust
fold_trust_root(repo, tip) -> Result<Option<TrustRoot>>
verify_candidate_trust_update(repo, old_tip, new_tip) -> Result<TrustRoot>
verify_commit(commit, trust_root) -> Result<KeyId>
verify_tag(tag, trust_root) -> Result<KeyId>
verify_push_cert(cert, repo_context, trust_root) -> Result<KeyId>
```

The candidate-update API is load-bearing: receive-pack and local sync both need to prove a new trust tip
before adopting it.

## Signed Trust Ref

Store repository trust state on:

```text
refs/gitana/trust
```

Each update is a signed commit whose tree contains a canonical trust document. The commit chain is the
authorization chain.

Rules:

- Bootstrap: accepted only when the first trust commit is self-signed by a key included in its own root.
- Update: accepted only when signed by a key trusted in the previous root.
- Removal/revocation: accepted only when signed by a still-trusted key.
- Empty-key roots are refused.
- `policy=require` should warn or refuse until at least two keys are enrolled, unless the caller passes
  an explicit break-glass flag.
- Concurrent edits use ref CAS. Divergence stops for manual reconciliation; do not auto-merge trust roots
  in v1.

## Receive-Pack Enforcement

Trust enforcement belongs before ref updates. A secure receive path should be:

```text
parse commands
unpack into quarantine
connectivity-check pushed tips
load and verify current trust root
verify candidate refs/gitana/trust update, if present
verify push certificate for protected refs
verify newly introduced signed commits and annotated tags
write objects
CAS refs
project caches and append audit events
```

Protected refs should include at least:

- `refs/heads/*`
- `refs/tags/*`
- `refs/gitana/workflows/*` or equivalent signed execution/config refs, if added later

Exemptions must be narrow:

- `refs/gitana/trust` is exempt from the push-certificate requirement only because its candidate chain is
  verified before the ref moves.
- Work-item or notes refs are not exempt unless they get their own signed operation log.
- Operator repair refs are disabled by default and require explicit local/server operator authority.

If the current trust root exists but cannot be folded, protected writes fail closed. The only allowed path
past that state is a deliberate repair command that records what happened.

## Push Certificates

A valid push certificate must prove the signer, target, commands, and freshness.

Verification checks:

- Signature verifies against a key trusted by the current root.
- Nonce is fresh and minted by this service.
- Nonce is bound to repo id, service, timestamp, and preferably random bytes or a server-side replay
  cache entry.
- `pushee` matches the canonical repository URL or repository id.
- Signed commands exactly match the commands receive-pack will apply.

Avoid timestamp-only nonces. Prefer:

```text
nonce = timestamp || random || HMAC(secret, timestamp || random || repo_id || service)
```

For multi-instance deployments, either share the HMAC secret and tolerate bounded replay inside the
freshness window, or store one-time nonces in a short-lived shared cache.

## Object Signature Enforcement

A signed push cert proves who moved refs. It does not prove every object is signed by an authorized author.
Treat object signatures as a separate check.

Under `require`:

- Every newly introduced commit reachable from protected refs must carry a trusted signature.
- Every newly introduced annotated tag reachable from protected tag refs must carry a trusted signature.
- Existing history can be grandfathered by an explicit baseline at the moment policy changes to `require`.
- Merge commits are ordinary commits and must be verified.
- Author key and pusher key do not need to match in v1, but record both for audit. A future policy can
  require equality or role-specific rules.

The implementation should walk only new reachability relative to the existing object graph to keep pushes
fast.

## Client UX

Add client support in small, reviewable slices:

```text
gta trust init --signing-key <key> --policy warn|require
gta trust list
gta trust add-key <pubkey> --signing-key <key>
gta trust remove-key <pubkey> --signing-key <key>
gta trust sync
gta commit -S
gta tag -s
gta push --signed
```

Client safety rules:

- Verify remote trust roots before updating local `refs/gitana/trust`.
- Never move local trust refs before candidate verification succeeds.
- Show policy and trusted key fingerprints after sync.
- Refuse `require` setup when no signing key is configured.
- Support Git config conventions where practical: `user.signingkey`, `gpg.format`, and
  `commit.gpgsign`.

## Audit And Recovery

Record audit events for:

- Trust root bootstrapped.
- Key added or removed.
- Policy changed.
- Signed push accepted or rejected.
- Unsigned or untrusted object rejected.
- Trust-root verification failed.
- Operator repair performed.

Recovery should be explicit:

- A local/server operator repair command may install a replacement root or roll back to the last known
  valid root.
- Repair requires filesystem/server authority, not ordinary repository write permission.
- Repair prints and records the old tip, new tip, reason, operator identity, and timestamp.

## Validation Plan

Minimum test coverage before enabling `require`:

- Stock Git SSH-signed commits verify.
- Stock Git GPG-signed commits verify, if OpenPGP support is included.
- Stock `git push --signed` verifies.
- Unsigned push is rejected under `require`.
- Signed push by an untrusted key is rejected.
- Signed push with stale or replayed nonce is rejected.
- Push certificate for repo A cannot be replayed to repo B.
- Trust-root update signed by a removed or untrusted key is rejected before the ref moves.
- Malformed trust root leaves the previous root usable, or fails protected writes closed if the current
  root itself is malformed.
- Unsigned commit inside a signed push is rejected under `require`.
- Unsigned annotated tag inside a signed push is rejected under `require`.
- Local `trust sync` does not adopt invalid remote roots.
- Fuzz pkt-line parsing, push-certificate parsing, commit/tag signature parsing, and trust-root JSON.

Useful focused commands should include:

```sh
cargo test -p gitana-trust
cargo test -p gitana-git-http --test push_cert
cargo test -p gta --test push_signed
cargo test -p gta --test trust
```

Add the exact tests as the crates land.

## Implementation Order

1. Extend `gitana-object` to preserve signed commit and annotated-tag payloads.
2. Add `gitana-trust` with signature verification and trust-root candidate folding.
3. Extend `gitana-git-http` push-certificate parsing/building and repo-bound nonce support.
4. Add pre-receive quarantine enforcement for candidate trust refs, push certificates, and signed
   objects.
5. Add `gta trust` commands and safe `trust sync`.
6. Add `gta commit -S`, `gta tag -s`, and `gta push --signed`.
7. Add `warn` mode, audit output, and baseline tooling for migrating existing repositories.
8. Enable `require` only after the validation matrix is green.

## Non-Goals For The First Secure Slice

- Threshold or quorum signatures for trust-root updates.
- Web-of-trust, CRLs, or automated key expiry.
- Confidentiality or repository encryption.
- SSH remote transport.
- Reproducible-build or CI provenance guarantees.

These can be layered later, but the v1 design should keep the pre-receive trust boundary strict enough
that later hardening does not need to undo the core flow.
