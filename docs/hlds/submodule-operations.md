# Submodule consumer operations

## Scope

Gitana implements the one-level consumer commands `submodule status`, `submodule init`, and
`submodule update`. The first release deliberately does not recurse into nested submodules and does
not implement clone-time recursion or the `merge`, `rebase`, or custom-command update strategies.
Unsupported strategies are rejected before initialization or filesystem mutation; `none` is an
explicit skip.

The implementation is split at an authority boundary. `gitana-submodule` owns declaration parsing,
selection, state transitions, validation, reports, and recovery. It receives already-opened
capabilities for the superproject's common directory, per-worktree git directory, and worktree. The
native frontend owns repository discovery, configuration layering, URL rewriting, credentials, and
HTTP, SSH, or local transport. Transfers are injected through `RepositoryTransfer`; the state
machine never discovers a transport or reads process environment.

## Repository layout

Each worktree owns its module repositories at:

```text
<per-worktree-git-dir>/modules/<submodule-name>
```

This rule applies unchanged to a main checkout and to a linked worktree. A module mount contains the
usual `.git` text file pointing to that exact repository. Names and paths are compared through a
lowercased Unicode canonical-decomposition key, so normalization-equivalent filesystem aliases are
rejected portably before mutation. Parent components, existing mount markers, and directory types
are validated without following symlink components. An existing marker is accepted only if its
lexically normalized target is the expected per-worktree repository; equivalent spellings are
rewritten to the canonical relative spelling. Windows marker containment and equality compare path
components with native case-insensitive semantics, so casing aliases of the retained module store
are accepted without weakening the namespace boundary.

The repository file-store routes the complete `modules/` namespace through the current worktree's
git directory. Standard gitlink consumers (`status`, `diff`, `ls-files -m`, and `add`) resolve a
mount marker against the discovered worktree root, require it to remain below that exact
per-worktree module namespace, and then read the module `HEAD` through the routed store. A linked
worktree's module movement is therefore visible and stageable without consulting the main
worktree's shared `.git/modules` directory.

The module repository's `core.worktree`, its mount marker, its index/worktree checkout, and detached
`HEAD` are maintained as separate publications. The module repository directory remains
identity-pinned across all four: config edits resolve `config` and any symlink target from that open
directory, and namespace replacement is rejected before marker publication and before completion.
An absent mount marker is published with an atomic no-replace operation against the exact no-follow
state captured before configuration work; an existing canonical or path-equivalent marker is
accepted without changing its bytes or identity. A marker that is added, replaced, or retyped during
publication is preserved and rejected. The config, marker, index, and `HEAD` are flushed before
completion intent is cleared. A foreign mount is rejected before `core.worktree` is changed. A
retained repository from
`submodule deinit` is reusable, but its attachment first writes a durable completion intent and is
forced through checkout even when the retained repository's `HEAD` already equals the
superproject's recorded commit. An already completed same-commit mount remains untouched, preserving
user worktree deletions like Git.

The mount directory is identity-pinned before any module transfer and reopened without following
symlinks immediately before publication. An identity replacement, newly added content, or a marker
state change during the transfer is a foreign-mount conflict; the retained handle is used for marker
and checkout work so a later path replacement is never overwritten. Clone, retained attachment, and
recovery population use exclusive additions rather than reset checkout: an exact target-compatible
partial entry can be resumed, while mismatched or additional content is preserved and rejected even
when it appears after marker publication. Each missing parent is opened or created component by
component without following symlinks. Regular-file content and mode are completed under a private
name, then the retained parent capability atomically publishes the final leaf only if it remains
absent. A concurrent final entry is preserved. Native cleanup removes the active private name by
moving the captured inode to an internal quarantine; it never follows that with a path-based unlink,
so a replacement of the quarantine name cannot be deleted. WASI lacks both no-replace rename and
delete-by-handle, so it creates the final leaf exclusively and writes through the retained descriptor.
It revalidates that descriptor before success and preserves a partial final leaf on write failure;
the error identifies the leaf for inspection or explicit removal before retry.

The first release's configuration model represents pointer values as UTF-8. If either relative
pointer cannot be represented exactly, update rejects the selected mapping during structural
preflight, before initialization, staging, configuration, or mount mutation; paths are never
lossily rewritten to replacement characters.

## Initialization and configuration

`init` performs a complete read/validation pass before its single compare-and-swap config edit. It:

- registers a missing `submodule.<name>.url`;
- activates the selected module;
- copies a safe `.gitmodules` update strategy when no local value exists; and
- resolves a leading `./` or `../` URL against the current branch's superproject remote only when a
  selected, unregistered declaration needs that resolution; when the selected remote has no URL, it
  warns and treats the superproject worktree root as its own authoritative upstream.

Command-line `-c` configuration is invocation-owned and overlays system, global, and repository
configuration without entering persistent config. A password embedded in a URL is never persisted
or included in structured/debug output. `update --init` may retain the original credential-bearing
URL only in private process memory for that invocation.

The superproject's effective configuration drives declaration URL rewriting, recursive transport
authorization, credentials, and connection setup. A newly staged module repository starts instead
from the ambient system/global/command stack; it never inherits the superproject's local or worktree
configuration as though those layers belonged to the module.

After `update --init` changes repository-local configuration, the state machine asks the native
frontend to rebuild the complete effective configuration stack. URL, activation, and strategy
decisions use that refreshed stack; raw local values never bypass worktree or command-scope
overrides. The same native authority owns the atomic local-config transaction: it follows a
symlinked config to its target, then opens each explicitly resolved directory component without
following links so a concurrent namespace replacement cannot redirect the edit. It locks and
re-reads the final target through a no-follow handle, preserves its mode, and publishes only while
that target still has the captured identity. A competing same-content inode is preserved rather than
overwritten, and the symlink itself is never replaced. The common-directory
capability used for initialization is retained from discovery, so
replacing its ambient path cannot redirect the transaction. Update strategy precedence is the
effective configured value, then the `.gitmodules`
declaration, then `checkout`; declaration `none` therefore remains an explicit skip even when
`update` runs without `--init`.

Config publication is conditioned on both the captured target identity and the exact lock entry
created by the transaction; replacing `config.lock` can neither install foreign bytes nor report
success. Config lock cleanup removes only that active name and retains the captured Unix inode in a
private quarantine. A module
`core.worktree` edit also returns an opaque publication token containing the inode it installed;
rollback consumes that token and refuses a same-content replacement inode instead of reverting a
competing writer's config.

Protocol authorization is also invocation-owned. Top-level clone/fetch/pull inputs use a
user-initiated policy; a URL read from `.gitmodules` or module config uses the stricter recursive
policy. `GIT_PROTOCOL_FROM_USER` and `GIT_ALLOW_PROTOCOL` are captured once at the command boundary,
and deeper layers do not consult ambient environment state.

## Update and recovery

`update` first validates the complete declaration namespace for unsafe, duplicate, case-folded, or
nested path/name collisions, then validates every selected mapping, gitlink, and strategy. It takes a
per-worktree advisory update lock and processes selected modules in index order, except that a module
owning a matching durable recovery intent is completed first so no other module can contend for the
shared staging namespace. On Unix, the guard also locks the retained per-worktree Git directory, so
renaming or unlinking the named lock cannot admit a second Gitana invocation into that namespace. On
Windows, the named lock is opened without delete sharing. The guard captures and revalidates the
named entry before and after recovery and module updates; namespace replacement fails closed while
the stable serialization guard remains held. Reports retain initialization effects and module
outcomes in actual completion order if a later module fails.

A new module repository is acquired without a worktree in the fixed private staging directory
`gitana-submodule-update/repository`. A versioned JSON intent records the module name, path, recorded
object ID, a credential-free source fingerprint, and, in version 4, its source context. A new
repository is `superproject`-scoped to the declaration endpoint; attachment of a retained repository
is `module`-scoped to the effective module origin that was fetched. Both contexts fingerprint the
selected endpoint after `insteadOf` rewriting and password removal, so rewrite changes and transport
usernames remain part of repository identity. Endpoint identity resolution performs no source I/O,
allowing a published repository to be validated while its source is offline. For a new repository,
the source is then authorized, opened, and format-checked before an intent or staging namespace is
published;
the prepared result must report the same resolved identity. The state machine records that intent,
opens the staging repository without following links, and gives the frontend that retained capability
for all repository writes. An initial HTTP or SSH transfer persists its rewritten endpoint with any
password removed, because a rewrite stored only in the superproject repository is outside the
existing module's later config boundary. A local transfer instead persists the canonical repository
root selected by exact-root inspection. It derives that spelling from the inspected layout rather
than resolving the ambient input again after retaining the source capabilities, so a retargeted
symlink cannot make the recorded origin disagree with the repository that supplied the clone.
Existing
module fetches likewise use the already-open module directory and its effective configuration;
ambient paths remain only diagnostic and relative-URL inputs. The prepared repository must use the
superproject's hash format and contain the recorded commit before an atomic no-replace rename
publishes it at `modules/<name>`. A raced final entry is preserved and leaves the private stage
recoverable. Before that rename, the recorded object graph (including its
pack index when packed) and the repository's current initialization metadata are flushed. Creating
the recovery directory is made durable by flushing both the intent directory and its per-worktree
git-directory parent before publication begins. After the repository rename, both its destination
module parent and the source recovery directory are flushed before mount publication begins.

The intent remains durable until the mount checkout and detached `HEAD` publication both succeed.
On retry:

- an intent with neither a staged nor a published repository is durably cleared and prepared from
  the current source, allowing correction of a URL rewrite, parse, or authorization failure that
  happened before repository creation;
- an unpublished staged repository with a matching intent is discarded and prepared again;
- a published repository with a matching intent is reopened and forced through checkout/HEAD
  completion without contacting its transport; its already-durable recorded object graph is
  validated locally, so a missing or corrupt graph fails closed rather than being refetched; and
- once either repository name exists, a mismatched intent, source, path, object ID, symlink, or
  ambiguous staged/published state is refused as recovery-required rather than guessed away.

Retry cleanup captures the staged repository and intent entry identities through the retained
recovery directory. Recursive stage deletion, intent publication, intent clearing, and stale lock
cleanup are each conditional on both captured source and destination entries, so a same-name
replacement is preserved and reported as recovery-required. After the active names are cleared, the
exact recovery directory is retired under a private name rather than recursively deleting its Unix
quarantines; the ordinary `gitana-submodule-update` name is absent for the next operation.

Versions 1 through 3 remain readable with their original superproject-scoped interpretation.
Version 1 and 2 intents remain readable only when their legacy fingerprint agrees with both the raw
configured source and the currently resolved endpoint. This preserves direct-source recovery while
failing closed for a legacy alias whose selected rewrite cannot be proven. When an accepted legacy
intent must reprepare an unpublished repository, its semantic identity is matched against the new
version 4 superproject intent and atomically upgraded before transfer; the serialized version alone
cannot leave a supported recovery state permanently blocked. A version 4 module-scoped intent is
valid only for a published retained repository. Recovery loads that pinned repository's effective
configuration and resolves its module origin from the module worktree base; a changed module origin
fails closed, while an unrelated superproject URL change cannot rebind or block the retained source.

Outside published-repository recovery, existing repositories perform a normal advertised-ref fetch
first. If the recorded commit is still absent, the frontend requests that exact object ID without
changing refs. Checkout uses a three-way worktree update for an existing module, preserving dirty
files and leaving `HEAD` unchanged when a conflict prevents the move. Before changing the index or
worktree, update retains `HEAD.lock` while preparing the detached-HEAD publication: it validates the
HEAD namespace, effective reflog policy, and reflog destination and snapshots the old value and
prospective reflog. The prepared capability
is consumed only after checkout, so a deterministic HEAD/reflog rejection cannot leave the files at
the recorded commit while `HEAD` still names the old commit. Merge, cherry-pick, revert, and rebase
state also block a move.
An ordinary mounted repository with an unborn or missing `HEAD` has no valid three-way base and is
refused without changing its config, marker, index, worktree, or `HEAD`; forced checkout remains
reserved for clones, retained-repository attachment, and recovery completion.

A bare relative local submodule URL is resolved from the superproject worktree root, never the
invocation subdirectory. The prepared module repository records the resolved canonical absolute
local source as its origin while the superproject retains its configured URL spelling. A direct
top-level relative local clone likewise records a canonical absolute origin and reflog URL; an
`insteadOf` alias supplied to top-level clone retains its original spelling.

Local clone publication uses no-replace namespace operations for both individual entries in a
pre-existing directory and the final directory rename for an initially absent target. A destination
that appears after reservation wins unchanged; cleanup discards only Gitana's retained empty
reservation.

An in-process local upload-pack also honours the source repository's own `shallow` file. That
boundary is propagated to clone, fetch, and module repositories, bounds negotiation and pack walks
even when parent objects remain present, and cannot be crossed by a deepen or unshallow request.
The send-side boundary and the receiving repository's metadata remain distinct: when a complete
client's offered-have closure already reaches a source boundary without crossing a client boundary,
the source still stops its pack walk there but does not tell that client to persist a shallow marker.
An exact-object want is subject to the same rule: a commit retained below the source boundary is
rejected unless another advertised ref exposes it within the source's visible history. Complete
non-shallow sources retain the exact-object fallback for unadvertised recorded commits. Only boundary
commits reachable from the requested wants are advertised to the client. In protocol v0 the boundary
constrains readiness and pack construction without completing negotiation: ordinary rounds without
`done` still return `multi_ack_detailed` acknowledgements and never a pack.

## MCP identity

The MCP surface derives schemas from the same clap command tree but owns tool identity and routing.
Top-level tool IDs retain their existing spelling (`hash-object`, `status`); nested executable tools
use their complete normalized path (`worktree_add`, `remote_set_url`, `submodule_status`). Registration
fails on a normalized collision instead of silently overwriting a tool. Root invocation options such
as `-C` and `-c` remain schema inputs and are rendered before the nested command path.

MCP subprocess execution is serialized by a guard owned by the blocking worker. Cancelling the
request detaches the already-started command but cannot release the guard or admit another command
until that worker exits. A failed subprocess result includes any completed stdout before stderr, so
the caller retains the successfully completed state-changing prefix.

Unauthenticated MCP HTTP serving accepts only loopback bind addresses. Host validation is paired with
an explicit Origin allowlist for `localhost`, `127.0.0.1`, IPv6 loopback, and the configured loopback
address at the listener's actual port; a supplied foreign browser Origin is rejected.
