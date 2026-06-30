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
- [ ] Stop `merge-base` from calling `std::process::exit(1)` (for a false `--is-ancestor` or "no common ancestor"). Exiting from command/library code would terminate a long-lived `gta-mcp` server. Return a typed outcome the front-ends map to an exit code, the way `merge` signals a conflict via `MergeConflict` rather than exiting.
- [x] Add a three-way tree merge engine. `Repository::merge_trees` recursively merges trees with diff3 line-level content merge (in the new `gitana-diff` crate); clean merges match `git merge-tree --write-tree` byte-for-byte, conflicts are reported per path. Not yet wired to the index/`merge` command.
- [x] Extend the index and status surfaces for conflict stages and unmerged paths. `Index` gains `conflict`/`unmerged_paths`/`is_unmerged`/`record_conflict` (and `upsert`/`remove` now collapse all stages, so `add`/`rm` resolve); `commit` refuses while unmerged; and `status` reports git's `UU`/`AA`/`DD`/`AU`/`UA`/`UD`/`DU` codes and now emits tracked changes before untracked (and both lines for a path that is staged-changed *and* untracked, e.g. after `rm --cached`) — oracle-tested against `git status` on real merge conflicts.
- [x] Add `merge` for fast-forward and true merge commits. `gta merge [-m] [--no-ff] [--ff-only] <commit>` fast-forwards or builds a two-parent merge commit (composing `merge_base`/`merge_trees`/`create_commit`/`reset_head`/`checkout`), oracle-tested against git (merged tree matches `git merge-tree`). A conflicting merge is refused cleanly; materializing conflicts (`MERGE_HEAD`, abort/continue) is the next item.
- [x] Teach `pull` to merge when fast-forward is impossible. `gta pull` now fetches then delegates integration to `merge` (fast-forward, or a true merge commit on divergence, with the same cleanliness/MERGE_HEAD/conflict handling) instead of erroring on a non-fast-forward. End-to-end coverage awaits the Smart HTTP test harness; the merge half is oracle-tested via `git_merge`.
- [x] Add abort/continue state for interrupted merge-like operations. A conflicting `gta merge` now materializes an in-progress state (`MERGE_HEAD`/`MERGE_MSG`, a conflicted index, work-tree markers) and exits non-zero; `gta merge --continue` (or `gta commit`) concludes it as a two-parent commit, `gta merge --abort` discards it. (cherry-pick/revert will reuse the same lifecycle.)
- [x] Add `cherry-pick`. `gta cherry-pick <commit>` re-applies a single non-merge commit (three-way merge against the picked commit's parent) as a single-parent commit preserving the author, with the shared conflict lifecycle (`--abort`/`--continue`/`gta commit`, via `CHERRY_PICK_HEAD`). The merge mechanics shared with `merge` now live in `commands/conflict.rs`. Multi-commit/range picking (a `.git/sequencer` todo-list, shared with rebase) is deferred.
- [x] Guard `merge` against silently deleting staged work. Matches stock git exactly: a fast-forward now applies only the HEAD→theirs diff via the new `WorkTree::twoway_merge` (git's `read-tree -m -u`), so unrelated staged/dirty files survive and a staged change to a touched path is refused (not clobbered); a true merge refuses any staged change (index must equal HEAD). Oracle-tested against `git merge`. **Follow-up:** `switch` and `reset --hard` use the same full `checkout` and share the staged-clobber exposure — they could adopt `twoway_merge` for the same fidelity.
- [x] Add `revert`. `gta revert <commit>` records a single-parent commit that undoes a non-merge commit (the reverse three-way merge: base = the commit, theirs = its parent), authored by the current user, with the git `Revert "<subject>"` message. Reuses the shared `commands/conflict.rs` lifecycle (`--abort`/`--continue`/`gta commit`, via `REVERT_HEAD`); merge commits, unborn/detached HEAD, empty results, and a dirty index are refused. Oracle-tested against `git revert`. Multi-commit/range forms deferred (the sequencer, shared with rebase).
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
- [ ] Add end-to-end clone/fetch/push tests over a local Smart HTTP harness.
- [ ] Add regression tests for SHA-256 repository config compatibility.
- [ ] Track unsupported Git features explicitly in README and this checklist.
- [ ] Periodically run the full workspace test suite before cutting milestones.
