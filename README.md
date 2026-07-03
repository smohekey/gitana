# Gitana

Gitana is a clean-room Git implementation written in Rust. It supports both the
SHA-1 and SHA-256 object formats, builds Git objects and protocol machinery
directly, and exposes the local command-line experience through `gta`.

The goal is not to wrap `git`. The core crates implement object codecs, object
storage, repository refs and revision walking, working-tree/index behavior, and
Smart HTTP protocol handling in-process.

## Current status

Gitana is usable for a focused Git workflow in either object format, but it is
not yet feature complete with Git.

What works today:

- Git-compatible repository initialization in either object format
  (`gta init --object-format=sha1|sha256`, default `sha256`).
- A hash-generic object layer (`ObjectId<H>`): the object model, codecs,
  packfiles, index, refs, and Smart HTTP protocol are all parameterized by the
  hash algorithm, with the concrete algorithm selected at runtime from a repo's
  config and negotiated with remotes over the wire.
- Loose object encoding/decoding for blobs, trees, commits, and tags.
- Packfile v2 decoding with OFS and REF deltas, plus packfile encoding.
- Object storage over local and memory file stores.
- Refs, symbolic `HEAD`, packed-ref reads, ref CAS updates, and reflog writes
  for commits and resets.
- Revision resolution for common forms such as `HEAD`, branch/tag names,
  abbreviated object IDs, `~`, `^`, and `^{type}`.
- A Git-compatible index (including conflict stages / unmerged paths), `.gitignore`
  handling, `status` (reporting `UU`/`AA`/`DD`/etc. conflict codes), `add`, `rm`,
  `mv`, checkout, path restore (working-tree and staged), soft/mixed/hard and
  path-limited reset, and staged/unstaged diff support.
- Smart HTTP advertisement, fetch/upload-pack, receive-pack, push reports, and
  fast-forward enforcement.
- A `gta-mcp` wrapper that exposes the same command implementations as MCP tools.

Major gaps:

- `gta merge` (and `gta pull`) does fast-forward and true (two-parent) merge
  commits. A conflicting merge materializes an in-progress state (`MERGE_HEAD`,
  `MERGE_MSG`, a conflicted index, work-tree markers); resolve it and conclude
  with `gta merge --continue` (or `gta commit`), or discard it with
  `gta merge --abort`.
- `gta cherry-pick <commit>` re-applies a single (non-merge) commit's change,
  preserving its author; `gta revert <commit>` records a new commit that undoes
  one (authored by the current user). Both use the same conflict lifecycle
  (`--abort` / `--continue` / `gta commit`, via `CHERRY_PICK_HEAD` /
  `REVERT_HEAD`). Multi-commit/range forms are not yet supported.
- `gta rebase <upstream> [--onto <newbase>]` replays the branch's commits onto a
  new base (linear histories only), with `--continue` / `--skip` / `--abort`.
  Non-interactive: no `-i`, autosquash, or `--rebase-merges`.
- There is no interactive rebase, stash, blame, bisect, submodule, hook, or
  sparse-checkout support.
- `checkout` switches branches and restores paths (`checkout [<tree-ish>] -- <paths>`),
  but switching to a detached commit is not yet supported.
- Push signing is still incomplete: the CLI has `--signed`, but key loading and
  signature generation are not wired through yet.
- Remote transport currently supports HTTP(S) Smart HTTP remotes. Other Git URL
  schemes, such as SSH remotes, are not implemented.
- Object storage now uses pack `.idx` and a multi-pack-index for lookup. `gta repack`
  consolidates into size-bounded packs (honoring `pack.packSizeLimit`); `gta gc` prunes,
  repacks *incrementally* (git's geometric strategy — keeping the large packs, as does
  `gta repack --geometric`), and writes a multi-pack-index reachability bitmap over the
  ref tips that stock git reads and trusts (`git multi-pack-index verify` /
  `rev-list --test-bitmap`). Gitana does not yet *consume* bitmaps to accelerate its own
  reachability queries (fetch negotiation, `rev-list`).

## `gta` CLI

Build the workspace, then run `gta` from Cargo:

```sh
cargo build -p gta
cargo run -p gta -- --help
```

Common local flow:

```sh
cargo run -p gta -- init demo
cd demo
printf 'hello\n' > hello.txt
cargo run -p gta -- add .
GIT_AUTHOR_NAME='A U Thor' \
GIT_AUTHOR_EMAIL='a@example.com' \
GIT_COMMITTER_NAME='A U Thor' \
GIT_COMMITTER_EMAIL='a@example.com' \
cargo run -p gta -- commit -m 'initial commit'
cargo run -p gta -- status
cargo run -p gta -- log
```

Implemented command groups:

- Plumbing: `hash-object`, `cat-file`, `ls-tree`, `rev-parse`, `rev-list`,
  `merge-base`, `ls-files`.
- Ref operations: `update-ref`, `symbolic-ref`, `branch`, `tag`.
- Working-tree porcelain: `add`, `rm`, `mv`, `status`, `commit`, `merge`, `log`,
  `show`, `switch`, `checkout`, `restore`, `reset`, `diff`.
- Repository setup: `config` (local read/write).
- Maintenance: `repack` (consolidate loose objects and packs; honors `pack.packSizeLimit`,
  splitting into multiple size-bounded packs when set; `--geometric` for an incremental
  repack), `prune` (delete unreachable loose objects), `gc` (prune, geometric repack, then
  write a multi-pack-index reachability bitmap over the ref tips).
- Remote operations: `clone`, `fetch`, `pull`, `push`, `remote` (list/`-v`, `add`,
  `remove`, `rename`, `set-url`).

## Crate layout

- `crates/core/gitana-object`: Git object model, loose object codecs, pkt-line
  handling, pack decode/encode, deltas, and graph enumeration.
- `crates/core/gitana-object-store`: Content-addressed object storage over a
  file-store backend.
- `crates/core/gitana-file-store`: Backend trait used by repository storage.
- `crates/core/gitana-file-store-local`: Local filesystem-backed file store.
- `crates/core/gitana-file-store-memory`: In-memory file store for tests and
  protocol state machines.
- `crates/core/gitana-file-store-conformance`: Shared file-store conformance
  tests.
- `crates/core/gitana-repository`: Repository semantics over objects and refs.
- `crates/core/gitana-worktree`: Git index, worktree scanning, status, add,
  checkout, and diff support.
- `crates/core/gitana-config`: Git config parser.
- `crates/core/gitana-diff`: Myers line diff and diff3 three-way line merge.
- `crates/core/gitana-git-http`: Transport-agnostic Smart HTTP protocol helpers.
- `crates/core/gitana-remote`: Remote operations over the `gitana-git-http` codec —
  origin config, ref discovery, the HTTP client, and pack transfer.
- `crates/cli/gta-core`: Shared command implementations.
- `crates/cli/gta`: User-facing CLI.
- `crates/cli/gta-mcp`: MCP wrapper around the same command surface.

## Compatibility checks

The test suite uses stock Git as an oracle where practical. These are useful
focused checks while working on the implementation:

```sh
cargo check -p gta --all-targets
cargo check -p gitana-git-http --all-targets
cargo test -p gta --test git_diff -- --nocapture
cargo test -p gitana-object --test git_index_pack -- --nocapture
cargo test -p gitana-worktree
cargo test -p gitana-git-http
```

The SHA-1 Git-oracle tests run against any `git` (SHA-1 is git's default format).
The SHA-256 oracle tests require a `git` binary with SHA-256 repository support;
where that is unavailable, those tests skip their oracle assertions and keep
exercising the native implementation where possible.

## Development notes

- The workspace uses Rust 2024 and forbids unsafe code at the workspace lint
  level.
- Gitana supports both the SHA-1 and SHA-256 object formats. The object layer is
  generic over the hash algorithm — object types are parameterised by `H` (e.g.
  `ObjectId<H>`, with `Sha1`/`Sha256` markers behind the `HashAlgorithm` trait).
  A repository's algorithm is read from its config at runtime and dispatched to
  the matching `H` (see `gta-core`'s `dispatch` module); remotes negotiate it
  from the advertised `object-format`. `gta init` defaults to SHA-256 but accepts
  `--object-format=sha1`.
- Keep command behavior honest in both `gta` and `gta-mcp`; both delegate to
  `gta-core`, but their argument surfaces differ slightly because MCP tools use
  named arguments.
- Prefer adding Git-oracle coverage when implementing behavior that stock Git can
  exercise locally.
