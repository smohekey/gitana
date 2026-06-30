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
- [x] Add a three-way tree merge engine. `Repository::merge_trees` recursively merges trees with diff3 line-level content merge (in the new `gitana-diff` crate); clean merges match `git merge-tree --write-tree` byte-for-byte, conflicts are reported per path. Not yet wired to the index/`merge` command.
- [x] Extend the index and status surfaces for conflict stages and unmerged paths. `Index` gains `conflict`/`unmerged_paths`/`is_unmerged`/`record_conflict` (and `upsert`/`remove` now collapse all stages, so `add`/`rm` resolve); `commit` refuses while unmerged; and `status` reports git's `UU`/`AA`/`DD`/`AU`/`UA`/`UD`/`DU` codes and now emits tracked changes before untracked (and both lines for a path that is staged-changed *and* untracked, e.g. after `rm --cached`) — oracle-tested against `git status` on real merge conflicts.
- [x] Add `merge` for fast-forward and true merge commits. `gta merge [-m] [--no-ff] [--ff-only] <commit>` fast-forwards or builds a two-parent merge commit (composing `merge_base`/`merge_trees`/`create_commit`/`reset_head`/`checkout`), oracle-tested against git (merged tree matches `git merge-tree`). A conflicting merge is refused cleanly; materializing conflicts (`MERGE_HEAD`, abort/continue) is the next item.
- [x] Teach `pull` to merge when fast-forward is impossible. `gta pull` now fetches then delegates integration to `merge` (fast-forward, or a true merge commit on divergence, with the same cleanliness/MERGE_HEAD/conflict handling) instead of erroring on a non-fast-forward. End-to-end coverage awaits the Smart HTTP test harness; the merge half is oracle-tested via `git_merge`.
- [ ] Add abort/continue state for interrupted merge-like operations.
- [ ] Add `cherry-pick`.
- [ ] Add `revert`.
- [ ] Add `rebase` once merge/cherry-pick primitives are solid.

## Object And Storage Performance

- [ ] Add pack `.idx` read/write support.
- [ ] Use pack indexes for object lookup instead of decoding whole packs eagerly.
- [ ] Add repack support.
- [ ] Add prune/gc safety rules for unreachable objects.
- [ ] Add multi-pack-index support when multiple packfiles become common.
- [ ] Add bitmap or reachability acceleration after pack indexing is stable.
- [ ] Resolve abbreviated object IDs across packed objects, not only loose objects.

## Remote And Protocol Parity

- [ ] Add HTTP authentication hooks compatible with ordinary Git credential flows.
- [ ] Add SSH remote support.
- [ ] Add `remote` command support for listing, adding, removing, and editing remotes.
- [ ] Add refspec parsing beyond the default `origin` fetch mapping.
- [ ] Support explicit push refspecs.
- [ ] Support tags in fetch and push flows.
- [ ] Add stock `git clone` interoperability tests against a small HTTP harness.
- [ ] Add stock `git fetch` interoperability tests against a small HTTP harness.
- [ ] Add stock `git push` interoperability tests against a small HTTP harness.

## Signing And Integrity

- [ ] Wire `gta push --signed` to load signing keys and produce real signatures.
- [ ] Add verification helpers for received push certificates.
- [ ] Add signed commit creation.
- [ ] Add signed tag creation.
- [ ] Decide how much signature verification belongs in core crates versus host policy.

## Working Tree Details

- [ ] Validate index paths in `checkout`'s removal loop, as `restore` now does. `checkout::run` calls `remove_worktree_path` on every index path not in the target tree without `validate_path`, so a hostile/corrupt index entry (`../x`) could delete a file outside the work tree. Fold the guard into `remove_worktree_path` (return `Result`) so both commands are covered.
- [ ] Acquire the index lock before mutating the working tree in `checkout`, `restore`, and `reset`, as `rm` now does via `WorkTree::lock_index`/`commit_index`. They mutate the working tree and only then `save_index`, so a held `.git/index.lock` fails after the tree has changed, leaving it inconsistent with the index. Take the lock up front so a locked index aborts before any filesystem change.
- [ ] Expand pathspec support beyond simple files, directories, and `.`.
- [ ] Add more complete `.gitignore` compatibility coverage.
- [ ] Add attributes support where it affects working-tree behavior.
- [ ] Add line-ending normalization support.
- [ ] Add sparse checkout support.
- [ ] Add submodule entry handling beyond tree mode representation.
- [ ] Add worktree command support for multiple linked working trees.

## User Experience

- [ ] Improve error messages for remote failures and protocol rejections.
- [ ] Add progress output for clone, fetch, push, and checkout operations.
- [ ] Add quiet/verbose flags where Git users expect them.
- [ ] Add porcelain output stability tests for command-line scripting.
- [ ] Add shell completion generation.

## Compatibility And Test Harness

- [ ] Keep adding stock-Git oracle tests for each implemented command.
- [ ] Add fixture repositories with branches, tags, merges, packed refs, and packs.
- [ ] Add end-to-end clone/fetch/push tests over a local Smart HTTP harness.
- [ ] Add regression tests for SHA-256 repository config compatibility.
- [ ] Track unsupported Git features explicitly in README and this checklist.
- [ ] Periodically run the full workspace test suite before cutting milestones.
