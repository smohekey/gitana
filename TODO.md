# Gitana TODO

Post-initial-commit checklist for growing `gta` toward broader Git parity.

## Command Surface Correctness

- [x] Fix `checkout` help and behavior mismatch: either make it branch-only, or add path restore semantics.
- [x] Add `restore` for working-tree and staged path restoration.
- [x] Add `reset` for soft, mixed, hard, and path-limited resets.
- [x] Add `rm` with tracked-file removal and staged deletion behavior.
- [x] Add `mv` as tracked rename convenience over filesystem move plus index update.
- [x] Add `show` for common commit/object display.
- [x] Add `config` read/write support for local repository configuration.
- [x] Make `config` writes preserve comments and layout. The config crate now retains each element's raw text and a value byte-span, so `set` edits the value in place and `add`/`unset` touch only their own line; comments and layout survive.
- [x] Keep `gta` and `gta-mcp` command surfaces in lockstep as each command lands. Enforced by a `surface_parity` test that compares a normalized spec of both clap command trees — recursively, including root/global args, and per argument its required-ness, action (arity), allowed values, defaults, and groups — whitelisting only the intended positional-vs-named presentation and the mcp-only serving flags.

## Merge And History Editing

- [x] Add merge-base computation. `Repository::merge_base`/`is_ancestor` (paint-down-to-common-ancestors with redundancy removal and octopus reduction) plus a `gta merge-base [--all] [--is-ancestor]` command, oracle-tested against stock git.
- [x] Stop `merge-base` from calling `std::process::exit(1)` (for a false `--is-ancestor` or "no common ancestor"). It now returns a typed `gta_core::SilentExit { reason }`; `gta` maps it to a non-zero exit with no output (git's convention) and `gta-mcp` surfaces `reason` as a tool error, so a library predicate no longer terminates the long-lived server. Mirrors the `MergeConflict` outcome pattern.
- [x] Add a three-way tree merge engine. `Repository::merge_trees` recursively merges trees with diff3 line-level content merge (in the new `gitana-diff` crate); clean merges match `git merge-tree --write-tree` byte-for-byte, conflicts are reported per path. Not yet wired to the index/`merge` command.
- [x] Extend the index and status surfaces for conflict stages and unmerged paths. `Index` gains `conflict`/`unmerged_paths`/`is_unmerged`/`record_conflict` (and `upsert`/`remove` now collapse all stages, so `add`/`rm` resolve); `commit` refuses while unmerged; and `status` reports git's `UU`/`AA`/`DD`/`AU`/`UA`/`UD`/`DU` codes and now emits tracked changes before untracked (and both lines for a path that is staged-changed *and* untracked, e.g. after `rm --cached`) — oracle-tested against `git status` on real merge conflicts.
- [x] Add `merge` for fast-forward and true merge commits. `gta merge [-m] [--no-ff] [--ff-only] <commit>` fast-forwards or builds a two-parent merge commit (composing `merge_base`/`merge_trees`/`create_commit`/`reset_head`/`checkout`), oracle-tested against git (merged tree matches `git merge-tree`). A conflicting merge is refused cleanly; materializing conflicts (`MERGE_HEAD`, abort/continue) is the next item.
- [x] Teach `pull` to merge when fast-forward is impossible. `gta pull` now fetches then delegates integration to `merge` (fast-forward, or a true merge commit on divergence, with the same cleanliness/MERGE_HEAD/conflict handling) instead of erroring on a non-fast-forward. End-to-end coverage awaits the Smart HTTP test harness; the merge half is oracle-tested via `git_merge`.
- [x] Add abort/continue state for interrupted merge-like operations. A conflicting `gta merge` now materializes an in-progress state (`MERGE_HEAD`/`MERGE_MSG`, a conflicted index, work-tree markers) and exits non-zero; `gta merge --continue` (or `gta commit`) concludes it as a two-parent commit, `gta merge --abort` discards it. (cherry-pick/revert will reuse the same lifecycle.)
- [x] Add `cherry-pick`. `gta cherry-pick <commit>` re-applies a single non-merge commit (three-way merge against the picked commit's parent) as a single-parent commit preserving the author, with the shared conflict lifecycle (`--abort`/`--continue`/`gta commit`, via `CHERRY_PICK_HEAD`). The merge mechanics shared with `merge` now live in `commands/conflict.rs`. Multi-commit/range picking (a `.git/sequencer` todo-list, shared with rebase) is deferred.
- [x] Guard `merge` against silently deleting staged work. Matches stock git exactly: a fast-forward now applies only the HEAD→theirs diff via the new `WorkTree::twoway_merge` (git's `read-tree -m -u`), so unrelated staged/dirty files survive and a staged change to a touched path is refused (not clobbered); a true merge refuses any staged change (index must equal HEAD). Oracle-tested against `git merge`. **Follow-up:** `switch` and `reset --hard` use the same full `checkout` and share the staged-clobber exposure — they could adopt `twoway_merge` for the same fidelity.
- [x] Add `revert`. `gta revert <commit>` records a single-parent commit that undoes a non-merge commit (the reverse three-way merge: base = the commit, theirs = its parent), authored by the current user, with the git `Revert "<subject>"` message. Reuses the shared `commands/conflict.rs` lifecycle (`--abort`/`--continue`/`gta commit`, via `REVERT_HEAD`); merge commits, unborn/detached HEAD, empty results, and a dirty index are refused. Oracle-tested against `git revert`. Multi-commit/range forms deferred (the sequencer, shared with rebase).
- [x] Add `rebase` once merge/cherry-pick primitives are solid. `gta rebase <upstream> [--onto <newbase>]` replays `<upstream>..HEAD` onto the base as fresh cherry-picks, with `--continue`/`--skip`/`--abort`. A multi-step sequencer persists `REBASE_*` state across invocations; conflicts reuse `commands/conflict.rs`; empty commits are dropped; linear histories only (a merge commit in the range is refused). No new detached-HEAD primitive — the branch is `reset_head`-ed to the base and advanced with `commit_on_head`. A shared `conflict::operation_in_progress` now interlocks merge/cherry-pick/revert/rebase. Oracle-tested against `git rebase`. Future: interactive (`-i`), autosquash, `--rebase-merges`.

## Object And Storage Performance

- [x] Add pack `.idx` read/write support. A hash-generic v2 `.idx` codec in `gitana-object`
  (`encode_pack_index`/`decode_pack_index`/`PackIndex`/`pack_index_entries`), oracle-tested
  byte-for-byte against `git index-pack`.
- [x] Use pack indexes for object lookup instead of decoding whole packs eagerly. `write_pack`
  now emits the `.idx` sidecar; the object store locates an object through the pack's `.idx`
  (id → offset, cached; a miss reads no `.pack`) and materialises just that object plus its
  delta chain via the new `decode_object_at`, so a pack is no longer decoded in full to serve
  (or miss) one object. A pack lacking its `.idx` falls back to a one-time full decode.
- [x] Add repack support. `gta repack` (backed by `ObjectStore::repack`) consolidates every
  loose object and all existing packs into one new pack, then deletes the now-redundant loose
  objects and old packs — data-preserving (no pruning). Writes the new pack before deleting
  sources; a no-op when already a single pack with nothing loose. Oracle-tested: stock `git fsck`
  reads the result and the full object set is unchanged.
- [x] Add prune/gc safety rules for unreachable objects. `gta prune` deletes loose objects
  unreachable from *every* root — refs, HEAD, the index (all stages), the reflogs (new
  `RefStore::reflog_object_ids` reader), `ORIG_HEAD`, and the merge/cherry-pick/revert/rebase
  heads — and refuses while an operation is in progress. `gta gc` prunes then repacks. Deletes
  only loose objects (packed objects are untouched); conservative and reflog-protected, so
  `reset`/`amend` orphans survive (reclaiming those needs future reflog-expiry). No mtime grace
  (the file store exposes no mtime). Refuses in a repository that has linked worktrees (their
  per-worktree roots aren't scanned — multi-worktree gc is future work) and requires a work tree.
  Oracle-tested against stock `git fsck`.
- [x] Add multi-pack-index support when multiple packfiles become common. A hash-generic v1 MIDX
  codec in `gitana-object` (`encode/decode_multi_pack_index`, `MultiPackIndex::lookup`); `repack`/`gc`
  regenerate `objects/pack/multi-pack-index` over the packs they produce (dropping it below two
  packs), and the object store consults it first — one binary search yielding `(pack, offset)` —
  scanning only packs it doesn't cover and falling back to per-pack `.idx` when absent/stale.
  Oracle-tested: stock `git multi-pack-index verify` accepts ours and we read git's.
- [x] Add bitmap or reachability acceleration after pack indexing is stable. A hash-generic
  EWAH codec plus a MIDX reverse-index (`RIDX`) chunk and reachability `.bitmap` reader/writer
  in `gitana-object`; a builder computes each ref tip's object closure and the type indexes;
  `ObjectStore::write_reachability_bitmap` writes the reverse-index-carrying MIDX + `.bitmap`,
  and `gta gc` calls it over the ref tips. Oracle-tested: stock `git multi-pack-index verify`
  and `git rev-list --test-bitmap` accept what gitana writes, and our reader reproduces
  `git rev-list --objects`. Not yet consumed by gitana's own reachability queries (fetch
  negotiation, `rev-list`) — that acceleration is future work.
- [x] Resolve abbreviated object IDs across packed objects, not only loose objects. `rev-parse`
  of a short id now resolves loose *and* packed objects: a new `ObjectStore::find_by_prefix` unions
  the targeted `objects/<aa>/` loose fan-out with a binary-searched range over the multi-pack-index
  (and each pack the MIDX doesn't cover), deduplicating an object stored in more than one place, so
  abbreviations keep resolving after a repack. `resolve_abbrev` now delegates to it and keeps the
  unique/absent/ambiguous decision.

## Remote And Protocol Parity

- [ ] Add HTTP authentication hooks compatible with ordinary Git credential flows.
- [ ] Add SSH remote support.
- [x] Add `remote` command support for listing, adding, removing, and editing remotes. `gta remote`
  lists the configured remotes (`-v` adds fetch/push URLs); `add <name> <url>` writes the
  `[remote "<name>"]` section with the default fetch refspec; `set-url` retargets it; `remove` drops
  the section and the remote's `refs/remotes/<name>/*` tracking refs; `rename` moves both (tracking
  refs, reflogs, symbolic-ref targets) and repoints the fetch refspec and branch/push-default config.
  Oracle-tested against stock git (git reads what gta writes; `git remote -v`/`rename` match).
- [x] Add refspec parsing beyond the default `origin` fetch mapping. A `Refspec` type in
  `gitana-remote` parses `[+]<src>[:<dst>]` (force `+`, a single `*` wildcard substituted src→dst,
  exact and empty-destination forms) plus negative `^<pattern>` exclusions; `gta fetch` reads all
  `remote.origin.fetch` refspecs and maps each advertised ref through them (first positive match wins,
  negatives excluded), enforcing fast-forward for non-forced refspecs and erroring on an exact source
  the remote does not advertise. `pull` honours a mirror refspec that maps into the checked-out branch
  (update-head-ok) and fast-forwards the work tree via merge. Oracle-tested (custom tracking namespace,
  non-fast-forward rejection, all-matching fan-out, conflicting-destination abort, checked-out-branch
  refusal, bare mirror, pull under a mirror refspec). Deliberate divergence: a non-fast-forward into
  the checked-out branch is refused (safe-error) rather than git's destructive force-reset. Still
  `origin`-only (no `gta fetch <remote>` arg yet) and the object download is a safe superset of the
  matched refs.
- [ ] Refuse a fetch refspec that updates a branch checked out in a *linked* worktree, not only the
  current `HEAD`. Fetch currently guards the current worktree's checked-out branch; git refuses a
  direct-mapping refspec (e.g. `+refs/heads/dev:refs/heads/dev`) whenever `dev` is checked out in any
  worktree, which needs enumerating `.git/worktrees/*/HEAD`. Belongs with the linked-worktree subsystem.
- [ ] Support explicit push refspecs.
- [ ] Support tags in fetch and push flows.
- [ ] Add stock `git clone` interoperability tests against a small HTTP harness. (The in-process harness in `git_smart_http.rs` is gta-to-gta; these still need stock `git` as the interop peer — likewise for fetch/push below.)
- [ ] Add stock `git fetch` interoperability tests against a small HTTP harness.
- [ ] Add stock `git push` interoperability tests against a small HTTP harness.

## Signing And Integrity

- [x] Wire `gta push --signed` to load signing keys and produce real signatures. `gta push --signed`
  attaches a push certificate signed with `ssh-keygen -Y sign` (git's `git` SSHSIG namespace) over the
  certificate body — the bytes stock `git push --signed` signs and receive-pack verifies. The porcelain
  `push` split into an unsigned `push` (wasm component, no signer authority) and a `push_signed` taking
  a pusher-identity resolver + `Signer`; the CLI resolves `--signing-key`/`user.signingkey` into a
  `LazyCliSigner` (explicit-ssh policy, like `commit -S`), both invoked only after the server advertises
  push-cert. Covered by a porcelain round-trip (cert verifies via the real trust core) and a CLI e2e
  over the loopback smart-HTTP harness.
- [x] Sign delete commands in `gta push --signed --delete`. `push_signed` now takes a `delete` target
  and, when set, sends a signed delete certificate (one command `<old> <zero> <ref>`) via the new
  `delete_signed`, so a `require` server verifies and authorises the deletion instead of receiving an
  unsigned delete. `build_cert` was generalised to `old`/`new` options (a `None` becomes the zero id);
  the CLI routes `--signed --delete` through `push_signed`. Covered by a porcelain cert round-trip and
  an `enforce.rs` verify_push acceptance test (complementing the no-cert rejection).
- [x] Emit a `trust sync` audit event. Added `AuditEvent::TrustRootAdopted { anchor }`;
  `TrustSyncOutcome::Updated` now carries the chain's bootstrap `anchor`, and the CLI sync handler
  prints the event to stderr on an adoption or fast-forward — completing the client-side audit
  vocabulary (descoped from step 7b). Scoped in
  `docs/hlds/trust-followups.md#completed`.
- [x] Add a one-time-nonce replay cache. Added a host-supplied `NonceLedger` capability (with a
  `NoReplayCheck` no-op default) threaded through the new `verify_push_with_ledger` / `ReceiveOptions`;
  after a certificate verifies, its nonce is recorded and a still-fresh replay is a certificate failure
  (rejected under `require`, warned under `warn`). The pure core stays stateless — the ledger is the
  host's state. `verify_push` delegates with `NoReplayCheck`, so its callers are unchanged. Covered by
  in-memory-ledger replay tests. Scoped in `docs/hlds/trust-followups.md#completed`.
- [ ] Add a persisted require-time baseline. Snapshot the grandfather set at the `require` cutover into a
  stored artifact so object-signature enforcement is incremental and stable, instead of the live
  `protected_baseline` walk. Scoped in `docs/hlds/trust-followups.md#persisted-require-time-baseline`.
- [x] Add OpenPGP signature support. Verify, enrol, and produce OpenPGP-signed commits/tags/push certs
  alongside SSHSIG, dispatching on the armor marker. `gitana-trust` verifies OpenPGP with a full
  certificate-validity engine (bindings, back-sig, key flags, expiry, reason-aware revocation) via the
  pure-Rust `pgp` (rpgp) crate; `gta trust add-key/remove-key` enrol armored PGP certs; `gta commit -S`/
  `tag -s`/`push --signed` sign via `gpg` when `gpg.format=openpgp` (or unset — git's default), with
  `gpg.openpgp.program`/`gpg.program` overrides. Interop-locked against stock git+GnuPG both directions.
  Trust-chain commits stay SSHSIG-only. Scoped in `docs/hlds/trust-followups.md#completed`.
- [x] Add verification helpers for received push certificates. `gitana-git-http`'s `verify_push`
  (`enforce.rs`) verifies a certificate's SSHSIG against the folded trust root, checks the repo-bound
  nonce freshness, matches the pushee, and confirms the signed commands equal what receive-pack applies.
- [x] Add signed commit creation. `gta commit -S` (and every history op) records a `gpgsig`-armored
  commit via the `signing` seam; verifiable by stock git and the trust core.
- [x] Add signed tag creation. `gta tag -a/-s` writes annotated tag objects, signed when `-s` (or
  `tag.gpgSign`), with the signature block preserved byte-for-byte.
- [x] Add trust audit output (step 7). A typed `gitana_trust::AuditEvent` vocabulary is emitted on both
  boundaries: `receive_pack` returns `audit: Vec<AuditEvent>` (push accepted/rejected, per-ref rejected,
  with `warn`-mode warnings), and the `gta trust` porcelain ops return `(tip, AuditEvent)` for
  bootstrap/key add-remove/policy change, which the CLI prints to stderr (kept off gta-mcp's captured
  stdout result). No persistence in v1; a host records the events however it wishes.
- [x] Decide how much signature verification belongs in core crates versus host policy, and enable
  `require`. Settled: `gitana-trust` owns pure verification; `gitana-git-http` orchestrates the
  receive-pack boundary; the host supplies the `TrustContext`. The full 8-step subsystem
  (`docs/hlds/secure-git-trust-signing.md`) is implemented and `require` is production-ready — the
  validation matrix is green (`docs/trust-validation-matrix.md`, including a real stock
  `git push --signed` verified end to end), with a migration preflight + guide
  (`docs/trust-migration.md`). Trust is opt-in per repository via the signed `refs/gitana/trust`
  root. Additive follow-ups (OpenPGP, a one-time-nonce replay cache, a signed `--signed --delete`, a
  persisted require-time baseline, and a `trust sync` audit event) are scoped in
  `docs/hlds/trust-followups.md` and tracked as their own items above.

## Working Tree Details

- [x] Validate index paths in `checkout`'s removal loop, as `restore` now does. `validate_path` is folded into `remove_worktree_path` (now `Result`-returning), so a hostile/corrupt index entry (`../victim`, `.git/…`) is refused rather than deleting a file outside the work tree; `remove_worktree_file` (the two-way-merge remove path) validates too. Regression-tested by `checkout_refuses_a_traversal_index_entry`.
- [x] Acquire the index lock before mutating the working tree in `checkout` and `restore`, as `rm`/`mv` do via `WorkTree::lock_index`/`commit_index`. Both now take the lock up front and `commit_index` on success / `release_index_lock` on error, so a held `.git/index.lock` aborts before any filesystem change (never leaving the tree inconsistent with an unwritten index) and a mid-apply failure does not orphan the lock. `reset --hard` materialises through `checkout` (covered); `reset --mixed`/path is index-only. Regression-tested by `checkout_aborts_before_mutating_on_a_held_index_lock`.
- [ ] Expand pathspec support beyond simple files, directories, and `.`.
- [ ] Add more complete `.gitignore` compatibility coverage.
- [ ] Add attributes support where it affects working-tree behavior.
- [ ] Add line-ending normalization support.
- [ ] Add sparse checkout support.
- [ ] Add submodule entry handling beyond tree mode representation.
- [x] Operate correctly inside linked working trees (`git worktree add`): resolve the `.git`-file pointer and route per-worktree files (`HEAD`, `index`) vs. shared common-dir files (`objects`, `refs`, `config`).
- [ ] Add a `gta worktree` command (`add`/`list`/`remove`) to create and manage linked working trees.

## User Experience

- [ ] Improve error messages for remote failures and protocol rejections.
- [ ] Add progress output for clone, fetch, push, and checkout operations.
- [ ] Add quiet/verbose flags where Git users expect them.
- [ ] Add porcelain output stability tests for command-line scripting.
- [ ] Add shell completion generation.

## Compatibility And Test Harness

- [ ] Keep adding stock-Git oracle tests for each implemented command.
- [ ] Add fixture repositories with branches, tags, merges, packed refs, and packs.
- [x] Add end-to-end clone/fetch/push tests over a local Smart HTTP harness. `gta/tests/git_smart_http.rs` runs `clone`/`fetch`/`push`/`pull` against a gta-served repo over an in-process Smart-HTTP loopback harness.
- [ ] Add regression tests for SHA-256 repository config compatibility.
- [ ] Track unsupported Git features explicitly in README and this checklist.
- [ ] Periodically run the full workspace test suite before cutting milestones.

## WASI / Component Target

- [x] Compile the object-database layer to a `wasm32-wasip2` component. `gitana-file-store-local` runs on an internal `Backend` trait — cap-std on native, `std::fs` on wasm (cap-std's WASI deps don't build on stable) — behind the async `FileStore` facade; `crates/demo/wasm-object-db` round-trips a blob under wasmtime. See `docs/hlds/wasi-capstd-file-store.md`.
- [x] Thread a capability root through `gitana-worktree` (its 60+ direct `std::fs` calls) so offline work-tree operations (`add`/`status`/`checkout`/`diff`) run in a component. Routed through a `WorkDirFs` capability (native `CapWorkDir`, wasm `DescriptorWorkDir`); `open-worktree` gained a third `work-dir` descriptor and the component exports `status`/`add`/`checkout`/`commit` (`gitana:repo@0.4.0`, `085f987`). WASI's silent exec-bit and minimal stat-cache are handled as documented degradation; symlinks round-trip. See `docs/hlds/worktree-capability-threading.md`.
- [x] Add an HTTP transport trait over a `wasi:http` host import so `clone`/`fetch`/`push` work on the wasm target, replacing `reqwest`. Dependency-free `HttpTransport` trait (reqwest behind a default feature, `3739fe1`); in-guest synchronous `WasiHttpTransport` over a trimmed vendored `wasi:http@0.2.12`; `fetch` (`gitana:repo@0.5.0`, `b927768`) and `clone`/`push` (`0.6.0`, `72cb5d5`) reuse the `gitana-porcelain` composites unchanged. Push *signing* stays deferred to the trust work. See `docs/hlds/wasi-http-transport.md`.
- [x] Author a first-class WIT world (`gitana:repo`, `crates/wasm/gitana-repo-component`): a reactor component whose `repository.open` export takes an **owned `wasi:filesystem` directory descriptor** as its capability — no preopen-path convention, no ambient opens. `gitana-file-store-local` gained a descriptor-backed `Backend` (`from_descriptor`, via the `wasip2` crate); hash detection runs in-guest through the descriptor (`gitana_repository::detect_hash_kind`); `crates/wasm/gitana-repo-host` proves it e2e under wasmtime 46 (no preopens, both hash formats, byte-identical objects). See `docs/hlds/wasi-component-porcelain.md`.
- [x] Grow the component's WIT surface to the full repo-level op set (`gitana:repo@0.2.0`): object reads (`read-object`/`-blob`/`-commit`/`-tag`, recursive `ls-tree`), revisions (`rev-parse`, `rev-list`, `merge-base`, `is-ancestor`), refs (`list-refs` incl. packed, `head`, CAS `update-ref`/`delete-ref`, symbolic refs), writes (`write-tree`, `create-commit`), `repack(geometric)`, `init(git-dir, kind)`, and `read-config` — with typed errors (`unknown-revision`/`ambiguous`/`ref-moved`/… via the core `InvalidRef` split). prune/gc stay excluded until worktree threading (their roots must include the index). See the addendum in `docs/hlds/wasi-component-porcelain.md`.
- [x] Two-descriptor component `open` (`git-dir` + `common-dir`) for linked worktrees (`WorktreeFileStore` over two `LocalFileStore`s instead of two cap-std `Dir`s). Landed as the additive `open-worktree` export (`gitana:repo@0.3.0`, `076373b`); host e2e proves the split byte-for-byte in both hash formats.
- [x] Resolve abbreviated object ids against packed objects too. `ObjectStore::find_by_prefix`
  unions the loose `objects/xx/` fan-out with a binary-searched range over the multi-pack-index
  (and each uncovered pack), so abbreviations resolve after a repack in both native and component
  paths (`7623339`). (Same work as the item under "Object And Storage Performance".)
- [x] Retire `StdBackend`/`from_root`: `wasm-object-db` takes its `/store` preopen as a descriptor (`wasi:filesystem/preopens#get-directories` → `from_descriptor`); the descriptor backend is the only wasm backend.
- [ ] Revisit WASI 0.3 (shipped 2026-06; Rust target still Tier 3): native async exports would delete the component's `block_on` shim.
- [ ] Stream large packfile writes through a single long-lived `spawn_blocking` pump instead of one per 64 KiB chunk (a micro-optimisation of the current correct, bounded-memory streaming).
