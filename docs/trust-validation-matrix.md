# Trust & Signing — Validation Matrix

Maps each item of the "Validation Plan" in `docs/hlds/secure-git-trust-signing.md` (the minimum
coverage required *before enabling `require`*, step 8) to the test(s) that cover it. This is a manual
map — when a test is renamed or moved, update the row.

Status legend: ✅ covered · 🟡 partially covered (a listed part is still pending) · ⚠️ deliberate
deviation, documented and accepted (not a gap) · 🔜 pending slice · ⛔ out of v1 scope.

`require` is production-ready (step 8e) only once every row is ✅, ⚠️, or ⛔ — i.e. no 🟡 or 🔜
remains.

| # | HLD validation item | Status | Covering test(s) |
|---|---|---|---|
| 1 | Stock Git SSH-signed commits verify | ✅ | `gitana-trust` `verifies_a_stock_git_signed_commit`, `verifies_stock_git_signatures_from_rsa_and_ecdsa_keys`, `verifies_a_merge_of_a_signed_tag_with_a_mergetag_header`, `verifies_signatures_over_non_utf8_messages` |
| 2 | Stock Git GPG-signed commits verify (if OpenPGP included) | ⛔ | OpenPGP is out of v1 scope (SSHSIG only); additive later |
| 3 | Stock `git push --signed` verifies | 🟡 → 8c | `gitana-git-http` `push_cert::verifies_a_real_git_push_certificate` proves a real captured cert verifies against the trust core. The full real-client → `receive_pack` loop is still pending slice 8c |
| 4 | Unsigned push is rejected under `require` | ✅ | `gitana-git-http` `enforce::require_rejects_unsigned_commit_and_missing_cert`, `enforce::wire_require_rejects_unsigned_push_and_leaves_ref_unmoved` |
| 5 | Signed push by an untrusted key is rejected | ✅ | commit by untrusted key: `enforce::require_rejects_commit_by_untrusted_key`; certificate by untrusted key: `enforce::require_rejects_a_cert_signed_by_an_untrusted_key` |
| 6 | Signed push with a stale or replayed nonce is rejected | ✅ stale / ⚠️ replay | Stale rejected: `enforce::require_rejects_stale_nonce`, `push_cert::nonce_accepts_fresh_untampered_and_rejects_otherwise`. Replay *within* the freshness window is accepted **by design** — a stateless HMAC nonce has no replay cache (the HLD's documented trade-off; a one-time-nonce cache is future work, not a v1 gate) |
| 7 | Push cert for repo A cannot be replayed to repo B | ✅ | `push_cert::nonce_accepts_fresh_untampered_and_rejects_otherwise` (nonce HMAC binds `repo_id`; a cert minted for repo A fails repo B's context) |
| 8 | Trust-root update signed by a removed/untrusted key is rejected before the ref moves | ✅ | `gitana-trust` `rejects_an_update_signed_by_an_untrusted_key`; `enforce::warn_still_hard_rejects_an_invalid_trust_update`, `enforce::accepts_a_valid_candidate_trust_update` |
| 9 | Malformed trust root fails protected writes closed if the current root itself is malformed | ✅ | `enforce::fails_closed_when_the_current_trust_root_is_unverifiable` |
| 10 | Unsigned commit inside a signed push is rejected under `require` | ✅ | `enforce::require_rejects_unsigned_commit_and_missing_cert`, `enforce::require_rejects_moving_a_protected_ref_to_an_unsigned_stored_commit`, `enforce::wire_require_partial_reject_applies_good_ref_and_ngs_bad` |
| 11 | Unsigned annotated tag inside a signed push is rejected under `require` | ✅ | `enforce::require_rejects_an_unsigned_annotated_protected_tag` (annotated tag object, no signature); `enforce::require_rejects_a_lightweight_protected_tag` (lightweight); `enforce::require_accepts_a_signed_annotated_protected_tag` (positive) |
| 12 | Local `trust sync` does not adopt invalid remote roots | ✅ | `gitana-porcelain` `trust::sync_refuses_a_divergent_remote_root_and_leaves_the_local_ref`, `trust::sync_bootstrap_confirm_error_propagates_and_leaves_the_ref_unset`, `trust::sync_declining_a_bootstrap_leaves_the_ref_unset` |
| 13 | Fuzz pkt-line, push-cert, commit/tag signature, and trust-root JSON parsing | 🔜 | slice 8b (`proptest` in-tree: no-panic on arbitrary bytes + round-trip stability) |

Supporting hard-invariant coverage (enforced regardless of policy, so `warn`/`off` cannot poison the
trust anchor): `enforce::off_still_hard_rejects_trust_ref_deletion`,
`enforce::rejects_trust_ref_deletion`, `enforce::rejects_a_mixed_bootstrap_and_protected_push`,
`enforce::rejects_a_trust_policy_change_mixed_with_protected_refs`,
`enforce::require_rejects_a_protected_branch_pointing_at_a_non_commit`.

## Remaining before `require` is declared production-ready

- **8b** — parser fuzzing (row 13).
- **8c** — full real-`git push --signed` → gitana `receive_pack` e2e (row 3, full loop).
- **8d** — migration preflight + docs for moving an existing repo to `require`.
- **8e** — flip the README/HLD status to production-ready once 8a–8c are green.
