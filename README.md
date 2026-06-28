# Gitana

Gitana is a clean-room Git implementation written in Rust. The project is
SHA-256-native, builds Git objects and protocol machinery directly, and exposes
the local command-line experience through `gta`.

The goal is not to wrap `git`. The core crates implement object codecs, object
storage, repository refs and revision walking, working-tree/index behavior, and
Smart HTTP protocol handling in-process.

## Current status

Gitana is usable for a focused SHA-256 Git workflow, but it is not yet feature
complete with Git.

What works today:

- Git-compatible SHA-256 repository initialization.
- Loose object encoding/decoding for blobs, trees, commits, and tags.
- Packfile v2 decoding with OFS and REF deltas, plus packfile encoding.
- Object storage over local and memory file stores.
- Refs, symbolic `HEAD`, packed-ref reads, ref CAS updates, and reflog writes
  for commits.
- Revision resolution for common forms such as `HEAD`, branch/tag names,
  abbreviated object IDs, `~`, `^`, and `^{type}`.
- A Git-compatible index, `.gitignore` handling, `status`, `add`, checkout, and
  staged/unstaged diff support.
- Smart HTTP advertisement, fetch/upload-pack, receive-pack, push reports, and
  fast-forward enforcement.
- A `gta-mcp` wrapper that exposes the same command implementations as MCP tools.

Major gaps:

- Merge and conflict handling are not implemented. `gta pull` is fast-forward
  only.
- There is no rebase, cherry-pick, revert, stash, blame, bisect, submodule, hook,
  or sparse-checkout support.
- `checkout` switches branches and restores paths (`checkout [<tree-ish>] -- <paths>`),
  but switching to a detached commit is not yet supported.
- Push signing is still incomplete: the CLI has `--signed`, but key loading and
  signature generation are not wired through yet.
- Remote transport currently supports HTTP(S) Smart HTTP remotes. Other Git URL
  schemes, such as SSH remotes, are not implemented.
- Pack lookup is simple: packs are decoded and cached as whole maps rather than
  using `.idx`, MIDX, bitmaps, or repack/gc machinery.

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
  `ls-files`.
- Ref operations: `update-ref`, `symbolic-ref`, `branch`, `tag`.
- Working-tree porcelain: `add`, `status`, `commit`, `log`, `switch`,
  `checkout`, `diff`.
- Remote operations: `clone`, `fetch`, `pull`, `push`.

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
- `crates/core/gitana-git-http`: Transport-agnostic Smart HTTP protocol helpers.
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

The Git-oracle tests require a `git` binary with SHA-256 repository support.
Where that is unavailable, tests that depend on stock Git skip their oracle
assertions and keep exercising the native implementation where possible.

## Development notes

- The workspace uses Rust 2024 and forbids unsafe code at the workspace lint
  level.
- Gitana is SHA-256-only by design. Repositories using SHA-1 object format are
  rejected rather than coerced.
- Keep command behavior honest in both `gta` and `gta-mcp`; both delegate to
  `gta-core`, but their argument surfaces differ slightly because MCP tools use
  named arguments.
- Prefer adding Git-oracle coverage when implementing behavior that stock Git can
  exercise locally.
