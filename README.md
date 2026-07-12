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
- A `wasm32-wasip2` component (`gitana:repo`, `crates/wasm/gitana-repo-component`)
  exporting the repo-level plumbing set — object reads/writes, revisions, refs
  (CAS updates), `repack`, and `init` — plus the working-tree porcelain
  (`status`, `add`, `checkout`, `commit`) over a repository opened with its
  working tree (`open-worktree`, which grants a third `work-dir` descriptor), and
  the Smart HTTP remote operations (`fetch`, `push`, and `clone`) over a
  host-granted `wasi:http` capability — no `reqwest`, no ambient network. A `401`
  challenge on those operations is authenticated the git way, with the credential
  answered by the host over a `credentials` WIT import (`fill`/`approve`/`reject`,
  the `wasi:http` capability model) — the component reaches for no ambient netrc,
  helper, or prompt. The component's only filesystem authority is the
  `wasi:filesystem` directory descriptors passed in by the host (no preopens, no
  ambient access).
  `crates/wasm/gitana-repo-host` embeds it under wasmtime; see
  `docs/hlds/wasi-component-porcelain.md` and `docs/hlds/wasi-http-transport.md`.
- A trust and signing subsystem (`docs/hlds/secure-git-trust-signing.md`) that makes
  protected-ref writes tamper-evident. Repository trust lives on a signed `refs/gitana/trust`
  commit chain carrying a policy (`off` / `warn` / `require`) and the trusted SSH keys;
  `gta trust init/list/add-key/remove-key/set-policy/sync` manage it — each update is re-verified
  before the ref moves, and `sync` adopts a remote root forward-only (pinning the bootstrap signer
  on first use). `gta commit -S`, `gta tag -s`, and `gta push --signed` sign with either format,
  chosen by git config `gpg.format` (`ssh` → `ssh-keygen`, `openpgp`/unset → `gpg`, matching git's
  default; programs overridable via `gpg.ssh.program` / `gpg.program`), interoperable with stock git
  in both directions. Receive-pack enforces the policy before any ref moves — verifying the candidate
  trust-root update, the push certificate (repo-bound nonce, pushee, exact commands), and a trusted
  signature on every newly introduced commit and annotated tag — failing closed on a malformed root
  and emitting typed audit events. Signatures (objects and push certificates) are verified in both
  SSHSIG and OpenPGP, dispatched on the armor, so a trust root may enrol OpenSSH keys, armored OpenPGP
  certificates, or both; trust-chain commits themselves remain SSHSIG-only. `require` is validated end
  to end against stock `git push --signed` (`docs/trust-validation-matrix.md`); migrate an existing
  repo onto it with the `--dry-run` preflight (`docs/trust-migration.md`).

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
- Trust signing produces and verifies both SSHSIG and OpenPGP signatures (git's `gpg.format`), but
  not yet OpenPGP's other forms (e.g. X.509/`gpgsm`). Trust-root updates are SSHSIG-only.
- Remote transport currently supports HTTP(S) Smart HTTP remotes. Other Git URL
  schemes, such as SSH remotes, are not implemented.
- Object storage now uses pack `.idx` and a multi-pack-index for lookup. `gta repack`
  consolidates into size-bounded packs (honoring `pack.packSizeLimit`); `gta gc` prunes,
  repacks *incrementally* (git's geometric strategy — keeping the large packs, as does
  `gta repack --geometric`), and writes a multi-pack-index reachability bitmap over the
  ref tips that stock git reads and trusts (`git multi-pack-index verify` /
  `rev-list --test-bitmap`). The **pack builder consumes** that bitmap: serving a fetch or
  push enumerates objects as `closure(wants) \ closure(haves)` over the bitmap (git's
  fill-in — a bitmapped commit contributes its whole closure at once, and only an
  un-bitmapped frontier is walked), so the have side is never read for a non-shallow fetch.
  Ancestry queries consume it too: fetch negotiation (`ok_to_give_up`) and `is_ancestor`
  (behind `merge`/`rebase`/`merge-base`) answer from a commit-only reachability set over the
  bitmap on a non-shallow repo. `prune`/`gc` liveness consumes it as well: the reachable-object
  closure that decides which loose objects survive takes the bitmap fast path on a non-shallow
  repo (the same walk-fill), falling back to the graph walk when shallow or un-bitmapped.
  `rev-list`/`log` still walk the graph — they need ordered output, which bitmaps do not provide.

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
- Linked working trees: `worktree` (`add`, `list [--porcelain]`, `remove [--force]`,
  `lock`/`unlock`, `prune`, `move`, `repair`). `add` creates git's admin layout and materialises the
  checkout — DWIMing a new branch named after the path's basename by default, with `-b`/`-B <name>`,
  `--detach`, or a `<commit-ish>` (checking out a branch by that name, else detaching), and refusing a
  branch already checked out in another worktree. `lock`/`unlock` write and remove `<admin>/locked`;
  `prune` drops the admin entries of worktrees whose checkout is gone (honouring locks and `--expire`).
  `move` relocates a checkout (repointing the admin backlink, `mv`-style destination, refusing a locked
  worktree without `-f -f`), and `repair` fixes the two cross-pointers after a manual move of a checkout
  or the main worktree. `move`/`remove` refuse a worktree holding an initialized submodule, and
  `worktree.useRelativePaths` pointers are preserved across a move/repair. The result is byte-for-byte
  git's layout, so stock git reads and operates in a gta-created worktree.
- Repository setup: `config`, scoped like git — `--local` (the repository `.git/config`, the default
  for writes), `--global` (`$GIT_CONFIG_GLOBAL`, else `~/.gitconfig` / the XDG file), and `--system`
  (`$GIT_CONFIG_SYSTEM`, else `/etc/gitconfig`). An unscoped read resolves across git's whole
  precedence stack (system → global → local, plus `-c`/`GIT_CONFIG_COUNT` command-line entries on top),
  and works outside a repository; an unscoped write lands in the local file. Writes are atomic through a
  `.lock` file, follow symlinks, and preserve the target's mode, matching git. Author/committer
  identity (and every reflog line) resolves `user.name` / `user.email` across the same stack, so a
  globally-configured identity is honoured — including by `clone`, which resolves its committer before
  a local config exists.
- Maintenance: `repack` (consolidate loose objects and packs; honors `pack.packSizeLimit`,
  splitting into multiple size-bounded packs when set; `--geometric` for an incremental
  repack), `prune` (delete unreachable loose objects), `gc` (prune, geometric repack, then
  write a multi-pack-index reachability bitmap over the ref tips).
- Remote operations: `clone`, `fetch`, `pull`, `push`, `remote` (list/`-v`, `add`,
  `remove`, `rename`, `set-url`). `fetch` honours the configured `remote.origin.fetch` refspecs —
  wildcard, exact, force (`+`), and negative (`^`) — mapping advertised refs to tracking refs and
  enforcing fast-forward for non-forced refspecs. A refspec that would write a branch checked out in
  any worktree — the current one or a linked one — is refused, as git does.
- HTTP authentication, matching git's credential flow: a remote that answers `401 WWW-Authenticate:
  Basic` is retried once with an `Authorization: Basic` header. Credentials resolve in git's order —
  URL userinfo (`https://user:pass@host`, with the password kept out of the saved `remote.origin.url`),
  then `credential.username`, then the configured credential *helpers* (`credential.helper` and
  per-URL `credential.<url>.helper` — `osxkeychain`, `store`, `manager`, …, invoked over git's
  `get`/`store`/`erase` protocol, honouring `credential.useHttpPath`), then an interactive prompt
  (`GIT_ASKPASS` → `core.askPass` → `SSH_ASKPASS` → the terminal, honouring `GIT_TERMINAL_PROMPT=0`
  and declining cleanly with no tty). A helper can supply a credential without prompting; on success it
  is handed back to every helper's `store`, and a rejected one to `erase`. `credential.<url>` matching
  is git's own (scheme, `*`-wildcard host, port, path-prefix at a `/` boundary; scheme-less patterns
  match exactly). A working credential is cached for the rest of the operation, so a clone/fetch/push
  authenticates once. (`url.*.insteadOf` rewriting is not yet wired.)
- Shallow history, matching git: `clone --depth N` / `--shallow-since <date>` / `--shallow-exclude
  <ref>` truncate the fetched history and record the boundary in `.git/shallow`. `fetch` then extends
  it with `--depth N` (absolute), `--deepen N` (relative to the current boundary), `--shallow-since` /
  `--shallow-exclude`, or fills it in completely with `--unshallow`. Ancestry walks (`log`,
  `rev-list`, `merge-base`, `rev-parse`) and `prune`/`gc` stop at the boundary rather than chasing the
  absent parents; `gc` skips the reachability bitmap for a shallow repo (as git does).
- Tags over the wire, matching git: `fetch` auto-follows tags (writing `refs/tags/*` for advertised
  tags reachable from the fetched branches), with `--tags` to mirror every tag, `--no-tags` to opt
  out, and `remote.origin.tagOpt` honoured; `push --tags` sends all local tags and `push
  --follow-tags` sends the annotated tags reachable from the pushed commits that the remote lacks.
  Existing tags are immutable (a non-forced update to one is rejected on both fetch and push), and a
  bare-name deletion (`push --delete v1`) resolves against the remote's refs, so it removes an
  existing `refs/tags/v1` rather than a nonexistent branch.
- Trust and signing: `trust` (`init`, `list`, `add-key`, `remove-key`, `set-policy`, `sync`;
  `init`/`set-policy` take `--dry-run` to preview a policy change). Commits and tags are signed with
  `commit -S` / `tag -s`, or automatically via git config `commit.gpgsign` / `tag.gpgSign` (with
  `user.signingkey` under `gpg.format=ssh`); a push is signed only when `push --signed` is passed —
  no config signs pushes automatically.

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
- `crates/core/gitana-trust`: Pure trust core — verifies SSHSIG- and OpenPGP-signed
  commits and tags against trusted keys, and folds the `refs/gitana/trust` commit chain
  (via an `ObjectSource` capability) into the effective trust root.
- `crates/core/gitana-repository-layout`: Ambient, native-only repository discovery — walks up
  to the containing repository (ordinary, linked worktree, or bare) and resolves its
  canonical layout (`RepositoryLayout`: worktree root, git dir, common dir). Reads only the
  filesystem — no `git` subprocess, config, or network.
- `crates/cli/gta-core`: Shared command implementations.
- `crates/cli/gta`: User-facing CLI.
- `crates/cli/gta-mcp`: MCP wrapper around the same command surface.
- `crates/wasm/gitana-repo-component`: `wasm32-wasip2` component exporting the
  repo-level command surface over a passed-in `wasi:filesystem` descriptor.
- `crates/wasm/gitana-repo-host`: wasmtime host harness and end-to-end tests
  for the component.

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
