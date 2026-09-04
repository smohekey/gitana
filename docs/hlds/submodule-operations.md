# Submodule consumer operations

## Scope

Gitana implements the consumer commands `submodule status`, `submodule init`, `submodule update`,
and `submodule deinit`. `status --recursive` and `update --recursive` explicitly recurse into nested
submodules; omitted flags remain one-level operations, and root pathspecs select only the first
level before every eligible descendant is considered. `clone --recurse-submodules[=<pathspec>]`
(also spelled `--recursive[=<pathspec>]`) publishes the root clone and then performs an initializing
recursive update over the selected submodules. The `merge`, `rebase`, and custom-command update
strategies remain unsupported.
Unsupported strategies are rejected before initialization or module filesystem mutation; `none` is
an explicit skip.

An implicit, no-path `submodule init` or `submodule update --init` follows root
`submodule.active` pathspecs when they are configured; without those root selectors it retains Git's
default of considering every gitlink. An explicit path remains an override and may initialize a module
outside the active set. Because a gitlink represents a directory, literal directory-form selectors
such as `modules/a/`, `modules/a/.`, and their exclusion forms match the gitlink itself. A
directory-only wildcard such as `modules/*/` or `:(glob)modules/*/` does not match a gitlink, in
accordance with Git's pathspec behavior.

Recursive status renders a depth-first pre-order and enters only initialized, non-conflicted module
worktrees. Recursive update first scans the currently initialized selected subtree and resumes
pending update journals deepest-first; any pending nested deinit fails closed. When the root owns a
durable update intent outside the caller's pathspecs, the original query and the owner's
top-relative literal query are evaluated independently and their status results are deduplicated for
this recovery scan. This preserves exclusion-only pathspec semantics while ensuring descendant
recovery completes before the root intent, without broadening the subsequent requested update. An
empty recorded owner path is malformed and fails recovery before a pathspec is constructed or any
recursive level is scanned.
Normal updates then complete a whole repository-local selection before
releasing every lease and entering successful children in level order. `Cloned`, `CheckedOut`, and
`AlreadyCurrent` parents are eligible;
unregistered, inactive, and `none`-strategy parents stop descent. This batch-first ordering is an
intentional safety difference from Git's sibling/descendant interleaving: no parent repository lock
is retained across a child operation. The root layout and its worktree, per-worktree Git-directory,
and common-directory identities are captured before ambient configuration or setup-lock waits and
revalidated on every recovery and update pass; a replacement root is never adopted as a fresh
recursive level. Each child is exact-root discovered and identity-revalidated after setup
serialization, and the core rejects a nested update journal that appears after the recovery scan but
before a parent would mutate that module repository. A durable recovery path is always interpreted
as a top-relative literal, independent of the caller's pathspec prefix or any wildcard and magic
characters in the recorded name. An empty update control directory with no staged repository is
retired directly under the per-worktree update lock and never converted into an all-module update.

Clone recursion has a deliberate root-publication boundary. Clone captures the worktree and `.git`
directory identities through the private destination capabilities before publication, then passes
that proof into recursive update. Before materialization, the root persists every requested
`submodule.active` pathspec, in command order, through the serialized repository-local config
transaction, including when none matches a gitlink in HEAD. A flag without a value records `.`;
repeated values are independent selectors, and exclusion-only sets retain normal pathspec semantics.
Unlike ordinary submodule commands, an unmatched positive clone selector is a successful no-op.
The requested values are validated independently, while root candidates are selected from the
freshly reloaded effective `submodule.active` set across every configuration layer. Consequently,
pre-existing system, global, and command-scope positive selectors remain additive instead of being
intersected with the clone command's locally persisted values. Root initialization omits modules
that remain inactive under that complete configuration: it does not read or validate their URL,
register, activate, transfer, or descend through them.
Descendants of successful active roots retain normal per-module activation. The updater reopens and
validates the visible root before reading or mutating it, so replacing the clone destination cannot
redirect recursion into another repository. Once the root is published it is caller-owned: a submodule
failure returns an error but retains the valid root clone, activation settings, completed module
prefix, and any durable update intent for a later `submodule update --init --recursive` retry. That
no-path retry reloads and applies the persisted root activation selectors, so it cannot register or
materialize a sibling excluded by the original clone request.
`clone --recurse-submodules --shallow-submodules` supplies an absolute depth of one to every selected
module and successful descendant. `submodule update --depth N` accepts only a positive depth and
applies it to newly prepared and existing module repositories, including the exact recorded-commit
fallback when that commit is older than the advertised branch tip. Recursive recovery carries the
current invocation's depth, but depth is intentionally not part of the durable source/target intent:
it changes local storage completeness rather than the semantic commit being recovered. Therefore a
failed shallow clone is faithfully continued with `submodule update --init --recursive --depth 1`;
a retry without the explicit depth may complete remaining repositories with full history. HTTP, SSH,
and `file://` sources honor the request. Initial native-local cloning ignores depth like top-level
clone, while later fetches into an existing native-local module honor it. Every advertised tip and
exact recorded-commit fallback requested by a shallow transfer crosses one combined object-graph
durability barrier with the complete shallow boundary before checkout/publication reports success.
A shallow root clone does not implicitly make its submodules shallow, and
`--shallow-submodules` without recursion has no effect. Clone-time jobs, `.gitmodules` recommended
shallow values, `--[no-]recommend-shallow`, shallow since/exclude propagation, explicit
`--no-shallow-submodules`, and remote-branch submodule updates remain unsupported.
Failures before root publication retain the ordinary clone cleanup contract.

The implementation is split at an authority boundary. `gitana-submodule` owns declaration parsing,
selection, state transitions, validation, reports, and recovery. It receives already-opened
capabilities for the superproject's common directory, per-worktree git directory, and worktree. The
native frontend owns repository discovery, configuration layering, URL rewriting, credentials, and
HTTP, SSH, or local transport. It also implements the injected native marker-target resolver: the
core passes the original marker text and retained expected repository capability, while only the
frontend may open an absolute or capability-escaping target and compare directory identities.
Transfers are injected through `RepositoryTransfer`; the state machine never discovers a transport,
opens an ambient path, or reads process environment.

## Repository layout

Each worktree owns its module repositories at:

```text
<per-worktree-git-dir>/modules/<submodule-name>
```

This rule applies unchanged to a main checkout and to a linked worktree. A module mount contains the
usual `.git` text file pointing to that exact repository. Recursive entry requires the requested and
discovered worktree roots to identify the same directory, and requires both the discovered
per-worktree Git directory and its resolved common directory to identify the expected module
repository. A foreign `commondir` redirect is rejected before identity capture, setup locking, or
configuration access, while native-equivalent spellings and aliases to the same directory remain
valid within the fixed layout. Names and paths are compared through a lowercased Unicode
canonical-decomposition key, so
normalization-equivalent filesystem aliases are rejected portably before mutation. Parent
components, existing mount markers, and directory types are validated without following symlink
components. An existing marker is accepted only if opening
its original, unnormalized target reaches the same directory identity as the expected per-worktree
repository. The original target is reopened after that comparison and its visible resolution must
still identify the retained repository. This accepts genuine native aliases while preventing either
lexical cancellation of a symlink component followed by `..` or a temporary namespace redirection
from establishing ownership. Equivalent spellings are accepted without rewriting their marker
bytes.

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
URL only in private process memory for that invocation. Recursive clone likewise retains its
credential-bearing root URL only for the post-publication update in that process. A relative child
may inherit that private base only when both its redacted resolution and the source selected from
serialized configuration resolve through `insteadOf` to the same password-free endpoint. The
private userinfo is applied only after that single rewrite, so it cannot suppress lexical rule
matching; a per-module override that selects another final endpoint remains authoritative. After a successful
network preparation, the full rewritten endpoint becomes only that child's private base for resolving
its relative descendants. Neither base enters configuration, reports, logs, or durable recovery, so
a later retry must acquire credentials normally.

The superproject's effective configuration drives declaration URL rewriting, recursive transport
authorization, credentials, and connection setup. A newly staged module repository starts instead
from the ambient system/global/command stack; it never inherits the superproject's local or worktree
configuration as though those layers belonged to the module.

After `update --init` changes repository-local configuration, the state machine asks the native
frontend to rebuild the complete effective configuration stack. URL, activation, and strategy
decisions use that refreshed stack; raw local values never bypass worktree or command-scope
overrides. Initial repository preparation receives that exact post-lock effective snapshot in its
`PrepareSource`; endpoint rewriting, authorization, credentials, and transport construction cannot
reuse a frontend configuration image captured before update acquired serialization. The same native
authority owns the atomic local-config transaction: it follows a
symlinked config to its target, then opens each explicitly resolved directory component without
following links and verifies the opened directory plus its still-visible name against the identity
captured before the open, so a concurrent directory or symlink replacement cannot redirect the edit. It locks and
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
the stable serialization guard remains held. Every detached config mutation worker receives a
cloned lease on that guard, including init edits, attachment edits and rollback, and deinit
reservation, preparation, restoration, and publication. Dropping the awaiting operation therefore
cannot admit another init, update, or deinit until its last blocking config worker has finished.
Pending update resumption acquires this guard before reading the durable owner and carries the same
guard through preflight, recovery, transfer, checkout, and intent retirement. If no journal remains
when the guard is acquired, resumption is a no-op; another compliant updater cannot clear the intent
between owner selection and the recovered update.
Reports retain initialization effects and module outcomes in actual completion order if a later
module fails.

Every command that reads shared repository configuration takes a non-mutating setup lease on the
capability-opened common `refs` directory. Setup leases are shared, never create a repository
entry, and therefore preserve ordinary inspection of a read-only repository. A config mutation takes
the exclusive form of that stable guard and additionally takes the replaceable
`gitana-submodule-config.lock` entry in the common Git directory. On Unix these are shared and
exclusive advisory locks on the retained `refs` directory handle; on Windows every stable handle
denies delete sharing while mutation handles additionally request delete access, making readers
shared and writers exclusive, and the named mutation entry is also opened without delete sharing.
The stable guard is deliberately distinct from every per-worktree Git-directory guard, including
when the main worktree's Git directory is the common directory, and from object storage that may be
shared by otherwise independent repositories.
The visible `refs` entry identity is captured without following its final component, then its
resolved directory is opened and identified before and after lock acquisition. The common-directory
capability and both identities remain attached to every setup or mutation lease. Replacing,
renaming, or retargeting `refs`, including a supported symlink or junction, invalidates the lease and
every update/deinit guard before another protected config read or namespace publication.
Acquisition is always per-worktree first, then the stable guard and, for mutation, the named entry.
Deinit then tries each selected module repository's own config-mutation guard, in selection order,
before validating or planning that module config. It retains those module guards through both config
publications and intent retirement. Detached module-config workers receive one lease combining the
superproject and module guards, so cancellation cannot admit a writer on either side of the
transition. While holding each module guard, parent deinit also rejects pending or malformed nested
update or deinit recovery owned by that module repository. A parent checkout containing a recoverable
child journal therefore cannot be retired, and a parent transition cannot replace the module config
inode pinned by a crashed nested transaction. The same checks precede active parent-deinit recovery.
Contention or nested recovery fails preflight without moving a mount or publishing an intent.
Update follows the same parent-before-module order for every published module repository: after
opening the retained module and before reading its hash or effective config, it tries that module's
config-mutation guard. The combined parent and module lease is retained through fetch, attachment,
rollback, and completion, so a stale module-local config writer cannot erase a successful
`core.worktree` publication. Contention fails before attachment with the normal update-lock error.
Module-scoped update recovery acquires that module guard before its first origin/config read, stores
the lease with the recovery plan, verifies that it covers the subsequently reopened module, and
reuses it through completion instead of reacquiring it.
`init`, `update --init`, `deinit`, config-writing porcelain, and setup-time config recovery all
participate. The mutation guard is acquired before config planning and held through publication or
journal retirement, and its lease follows detached config workers, so linked worktrees cannot plan
concurrent transitions from the same shared-config inode. Setup leases wait for that complete
publication rather than observing a temporarily absent Windows config. Standalone `init` first
rejects recovery, reloads, and plans under a non-mutating setup lease. If the complete selected
batch needs no config update, it returns without opening or creating either mutation lock;
otherwise it releases the reader, acquires the mutation guard, rejects recovery again, reloads, and
replans before publication.
`update --init` retains its existing single planning pass under the update mutation guard.
While that guard is held, `init` and `update --init` reject a pending deinit intent in any linked
administrative directory; `deinit` may resume its own intent but rejects one owned by another
worktree. A crashed owner therefore remains the sole authority for its recorded shared-config
identity. Linked-worktree removal, relocation, and non-dry-run pruning take the same common guard,
reject pending or malformed deinit state in every administrative directory, and retain the guard
through their namespace mutation. Force flags cannot override this recovery boundary. Listing and
dry-run pruning remain serialized read-only operations.

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

## Deinitialization and recovery

`deinit` requires an explicit `--all` or one or more paths. It validates every selected mapping,
namespace, module repository identity, expected `core.worktree` attachment, and non-forced checkout
before mutating the first module, then completes modules sequentially in index order. A later failure
reports the already-completed prefix. `--force`
bypasses only the content-cleanliness proof; it never permits a foreign mount, marker, repository,
repository format, or config attachment to be removed, and it does not destroy the displaced
checkout. Repository-format validation also applies to already-unmounted modules. A mounted
directory's exact identity plus the marker file's identity and bytes are captured during preflight.
Marker-target resolution is followed by a second no-follow identity-and-bytes read, and the complete
snapshot is revalidated after awaited config planning, immediately before it is recorded in the
journal.

While holding the mutation guards, deinit reads and completes an active owning intent before loading
the current index or declarations. The journaled name, path, gitlink object ID, force flag, and
transition proofs remain authoritative even when an intervening checkout, switch, or index edit has
removed or replaced that gitlink. A recovered path satisfies matching request pathspecs and is not
planned again from its current index entry during the same invocation; unrelated unmatched
pathspecs retain their normal error. Any later fresh-selection failure reports the completed
recovery outcome.

The expected module attachment is the highest-precedence repository-owned `core.worktree`, excluding
ambient command, environment, and user configuration. The editable base `config` remains the sole
transition target and must name the same mount. Relative values are resolved from the module Git
directory; lexical normalization, native path equivalence, and canonical identities for existing
paths allow absolute, dotted, and platform-alias spellings of that mount. Canonicalization operates
on the original joined path before any lexical normalization, so a symlink followed by `..` retains
its filesystem meaning. If either path does not yet exist, lexical comparison is permitted only for
a spelling with no normal component before a parent component. Only a genuine missing-path error
permits that fallback; permission failures, symlink loops, identity changes, and all other resolution
errors fail closed. A successfully resolved side must also prove an absent endpoint before it can be
paired with a missing-path fallback. The transition persists the exact raw value, resolved parent
identity, resolved target identity, and ordered symlink identities from the capability-pinned
resolution. Reservation and preparation revalidate that proof. Publication
performs the same resolution again inside the config provider's blocking worker, immediately before
the namespace replacement, and requires the current configured target to identify the currently
selected public mount. A retargeted alias therefore cannot turn a previously valid attachment into
a foreign attachment during the async-to-blocking handoff. If
`extensions.worktreeConfig` enables a `config.worktree` value for `core.worktree`, deinit rejects the
module even when the override names the same mount: a one-target version-4 transition cannot prove
that the effective attachment has been removed. It likewise rejects an included occurrence that
would survive unsetting the base file's own value. The effective repository-owned attachment is
revalidated before reservation, preparation, and publication and must be absent after publication;
a concurrent override leaves the durable intent pending.
During all-selection preflight, a mounted module is also rejected when either the resolved module
base-config target or the shared superproject-config target is inside any selected checkout. The
containment decision covers the final target parent and every directory traversed through ordinary,
parent, and symlink components, all from the same pinned resolution used for the identified config
read. After every module
guard and checkout capability is retained, batch preflight checks every planned or journaled module
target against every selected checkout before recovery or mutation. Separate ambient
canonicalizations therefore cannot mix a temporary decoy with the planned target, and a module
config cannot silently depend on a later checkout in the same batch. Detachment would make such a
symlink unreachable before the current durable config reservation can reopen it. External symlink
targets remain supported and identity-pinned; supporting a checkout-contained target requires a
future journal format that can address the target after relocation. The same all-selection preflight
also compares the final file identities of every planned or journaled module transition with one
another and with the shared superproject target. Any alias is rejected before recovery or namespace
mutation: independently publishing complete config images against one inode would make the first
publication invalidate the second transition after checkout retirement.

While those module guards and checkout capabilities are retained, deinit also rebuilds every
selected module's effective configuration with a containment-aware native resolver. Repository and
worktree layers and every include path consulted by include expansion or its remote-URL pre-scan
must resolve without traversing any selected checkout, including a path that enters a checkout
through ordinary directories and later escapes with `..`. This validation runs before an intent or
mount mutation. External effective-config inputs remain supported; checkout-contained inputs are
rejected because the secret-free journal does not persist their contents and cannot reopen their
public names after displacement. A guarded native read returns the real parent derived during that
same capability-pinned component walk; layer and include resolution never ambient-canonicalize the
path again after reading its bytes. Config conditions therefore cannot combine one target's bytes
with a concurrently substituted target's directory.

Creating and flushing the deinit control directory precedes intent publication. If a crash leaves
that directory without an active intent and the affected gitlink is later removed, an explicit
`deinit --all` with an empty selection identity-retires the exact control directory while holding
the mutation lock. Private or partial debris remains reachable under its retired name, and the
active control name no longer blocks unrelated config writers. A present or malformed intent still
fails closed and can only be advanced by its owning recovery transition.

For a mounted module, deinit publishes a durable, secret-free intent before replacing the public
mount with an empty sibling directory. Every native platform uses two identity-checked,
no-replace renames with separate prepared, displaced, and rollback names. The intent advances after
the checkout displacement and after empty-directory publication, so either intermediate namespace
is recoverable. A source identity is revalidated before the rename; if it changes in the remaining
syscall window, recovery restores the observed entry only to its still-vacant original name and
otherwise preserves both entries with the intent pending. Legacy version-4 Unix intents whose
prepared and displaced names alias are normalized durably from their exact observed pre-exchange,
post-exchange, or rollback state before another namespace mutation. The displaced checkout stays
under its private sibling name until its recorded directory identity and exact marker identity and
bytes have been revalidated through that relocated capability. Marker targets are resolved only
while the mount is still at its validated public path; recovery never re-resolves a relative marker
against the now-empty former path. The separately pinned module repository identity supplies the
other half of the ownership proof. Without `--force`, status and `HEAD` are also revalidated. A clean
checkout must have `HEAD` at the superproject's recorded gitlink commit
both before displacement and before retirement; a different or missing `HEAD` is a local
modification unless `--force` was requested. Status includes visible untracked and staged changes,
while the
retirement proof separately hashes tracked content so stat-cache and skip-worktree edits cannot be
lost. Worktree-relative `core.excludesFile` paths and absolute paths component-wise beneath the known
worktree root are read through the opened original or displaced checkout rather than re-resolved
through the empty public mount. External absolute paths and the XDG default remain ambient. Ignored
content does not block deinit and is retained with the rest of the checkout. A missing excludes
file, including one below a missing intermediate directory, contributes no patterns just as it does
in Git. A configured directory remains fatal even on Windows, where attempting to open that
directory as a regular file can initially report `PermissionDenied`; the target type is checked
before that error is treated as an unreadable file.

The version-4 intent records its transaction phase, the module repository and mount identities, the
accepted marker file identity and exact bytes, every private namespace name, and a complete resolved
config descriptor plus before/after SHA-256
fingerprints for each module and superproject transition, never raw config or credentials. The
descriptor pins the resolved parent directory, the target file or planned absence, and every
symlink in resolution order. A legacy version-4 mounted intent without a marker snapshot locates the
recorded mount identity only among its journaled public, displaced, or retired names, validates the
marker, and durably rewrites the same phase before semantic recovery mutates another namespace. A
changed config first reserves and fsyncs an empty private sibling
inode. Its identity is journaled before any rendered bytes are written into that exact reservation;
only a later journal phase permits publication through the pinned namespace. Recovery can therefore
rewrite a partial reserved image, but accepts only the planned target, the recorded reservation, or
that inode already published as the target. Same-content replacements, hard-link redirections
through a retargeted symlink, and replaced resolved parents fail closed. Every namespace or config
publication, including recovery that observes an already-published config inode, flushes the target
directory before advancing the journal durably. On Windows, config replacement first moves the old
target to the journaled reservation name and flushes that displacement, then publishes the prepared
inode from `config.lock`, flushes it, and removes only the recorded old inode. Recovery recognizes
the displaced-before-publication and published-before-cleanup states without an unrecorded backup.
Shared current-repository command setup for every CLI and MCP repository or worktree command retains
the common config lock through hash detection and effective-config loading. It does not take the
per-worktree mutation lock, and its distinct Unix `refs` guard does not alias the long-lived update
directory guard, preserving the established update-lock error precedence without making setup wait
for an ordinary update. Common-lock acquisition waits in a blocking worker, so a concurrent Windows publication cannot
expose its deliberately absent target to those reads. While retaining serialization, setup
capability-enumerates the common directory and every linked per-worktree administrative directory,
including when discovery starts from a bare common repository, and deduplicates them by directory
identity. It inspects every journaled prepared module and superproject config target and accepts only
the exact before image, exact prepared after image, or exact absent publication gap. For an absent
target it releases the read lease, takes both the owning intent's per-worktree lock and the common
config lock, and restores the exact journaled old inode while preserving the exact prepared inode at
`config.lock`. Setup then reacquires its read lease and repeats the inspection before loading
configuration. If the owning worktree update lock is still held, setup waits asynchronously with a
10 ms exponential backoff capped at 250 ms before reacquiring locks or rescanning journals. The
backoff resets after a successful restoration; other recovery errors still fail closed. A foreign
resolution or inode fails closed.
Commands that read or replace the repository-local config after setup retain that same common lock
through their complete read-modify-write transaction. This includes repository config and remote
commands and sparse-checkout's `extensions.worktreeConfig` publication, so none can overwrite a
concurrent deinit transition with a stale config image. Their local file-store backends also retain
an opaque clone of that lease: every queued blocking CAS or replacement worker captures the backend,
so cancelling the CLI or MCP future cannot release common-config serialization before an already
queued namespace publication finishes. Before a repository-local config write begins, the command
enumerates the current and linked-worktree administrative directories while still holding that
common lock and rejects any pending or malformed deinit recovery state. A durable pre-publication
intent therefore keeps exclusive authority over its recorded config inode. Serialized config reads,
remote listing, and sparse reapply remain available because they cannot replace that inode. After
winning its per-worktree update lock, a plain `submodule update` takes a fresh shared setup lease and
reloads the effective configuration before deciding registration, activity, URL, or strategy. A
deinit that completed between command setup and update serialization therefore yields
`SkippedUnregistered` instead of being undone from the stale setup image. `update --init` still
reloads after initialization while holding its common config mutation lock, because initialization
may have just changed registration.
`submodule status` retains the superproject setup lease and, in parent-before-module order, acquires
each selected module repository's own read-only setup lease through its config, hash, and HEAD read;
the frontend opens its common, Git, and worktree capabilities only after acquiring that first lease
and exact-root revalidating the discovered layout. A command that waited behind parent deinit
therefore cannot retain or reopen the retired checkout. Status also records each visible module Git
directory identity before waiting, reopens and compares that directory after acquiring its module
lease, and revalidates both the lease and visible directory after its hash and HEAD reads. A renamed
or replaced `modules/<name>` repository is rejected instead of being reported through a detached
capability. `init`, `update`, and `deinit` release that reader before taking their exclusive guards.
Fetch reads the repository-owned `core.bare` value from the capability-pinned, symlink-aware
effective config under setup serialization and passes that snapshot into porcelain ref-selection,
then releases setup before shallow validation, identity lookup, or network and object transfer.
External config symlinks and worktree-local repository layers are therefore honored without letting
system, global, or command-scope values redefine repository identity. Fetch cannot misclassify a
bare repository during Windows publication without extending the setup critical section across I/O.
Pull likewise passes its serialized repository-local `core.bare` snapshot into both checkout
enumeration and porcelain fetch validation, then releases setup serialization before network fetch.
Once fetch completes and the upstream is resolved from the pre-fetch configuration snapshot, pull
reacquires setup serialization and performs exact-root discovery of the original public worktree.
Its canonical layout and the recorded worktree, per-worktree Git-directory, and common-directory
identities must equal the pre-fetch proof, so even a replacement that recreates the same marker and
administrative paths fails before integration. Pull constructs the merge worktree from the exact
capabilities validated against that proof and binds the lease to detached file-store workers.
Signing, identity resolution, sparse and file-mode
reconciliation, checkout, and ref publication therefore remain serialized through post-fetch
integration without holding the common config guard across network I/O.
Signed push keeps the setup lease attached to the originally opened repository and resolves the
lazy certificate identity through that retained repository capability. The identity is still not
read unless the server accepts signed pushes, while a rename or path replacement cannot mix the
original refs and objects with another repository's signer identity.
Ordinary worktree dispatch discovers only the layout and path prefix before acquiring its shared
setup lease. Before waiting it records the worktree, per-worktree Git-directory, and common-directory
identities. While holding the lease it exact-root revalidates both the canonical layout and those
identities, then consumes the exact checked capabilities. Hash selection, pending-recovery gating,
the repository-local config, and `config.worktree` are all read through those retained directories;
the canonical paths remain only for Git path-matching semantics and diagnostics. An empty
replacement, moved checkout, rebound repository, or same-layout inode replacement therefore fails
before command execution, while a replacement installed after revalidation cannot be mixed with
the retained repository. Dispatch retains the lease through
the complete command future and passes a clone to detached file-store workers. Status, switch,
reset, remove, move, checkout, and other worktree operations therefore serialize every deferred
`core.fileMode`, sparse-checkout, and repository-config read without requiring a command-by-command
allow-list.
Repository-only dispatch follows the same rule, including bare repositories: it records the optional
worktree plus per-worktree and common Git-directory identities before waiting, exact-root revalidates
them under setup serialization, and supplies hash detection, effective-config loading, and pending
recovery checks only from those checked capabilities. Direct fetch, pull, push, and trust flows carry
the same proof across network pauses and revalidate it after every later setup wait. Setup bootstrap
also enumerates and restores displaced configuration through these revalidated directories, so a
replacement repository cannot be touched before the caller's identity check fails.
Configuration writers exact-root revalidate first and pass that checked common-directory capability
into the blocking mutation-lock acquisition. The replaceable named lock is therefore created only in
the repository captured before the wait; a second exact-root validation after acquisition rejects a
repository replaced while the worker was blocked.
Both images and their pinned resolution are validated. Missing linked-worktree administrative
containers are treated as empty, and unrelated non-directory children are ignored. A symlinked or
otherwise uninspectable `worktrees` container, or a symlinked administrative child, rejects the
entire enumeration instead of yielding a partial owner set. Every opened directory identity is
also compared with the still-visible name before recovery authority is accepted. Read-only
`worktree list` is the narrow exception: when the resolved target of the shared config is present it
retains setup serialization but delegates directly to the linked-worktree collector, which omits
symlinked administrative entries and cannot mutate or invalidate a hidden recovery owner. Config
symlinks are followed and revalidated through the retained common-directory capability for this
presence check. If the resolved target is absent, listing uses normal recovery-aware setup and fails
closed when owners cannot be enumerated.
Exact-root inspection of a repository used only as a local transport source remains read-only and
does not bootstrap that source. Clone, fetch, pull, and submodule transfer acquire a non-recovering
setup lease after exact-root inspection. They record the source's optional worktree, per-worktree
Git-directory, and common-directory identities before waiting, revalidate the layout and identities
under the lease, and retain the resulting capabilities through hash detection and effective-config
loading. A path replacement during the wait is rejected rather than accepted as the source. A
submodule update passes its retained config-mutation lease to the transfer boundary; when
the exact source common-directory identity is already covered by the superproject or current module
lease, the source read reuses that lease instead of recursively acquiring its shared form. Unrelated
sources still acquire an independent setup lease. A standalone clone, fetch, or pull waits for a live
source mutation. A submodule transfer that already retains an unrelated mutation lease tries the
source setup lease without waiting and reports the normal update-lock error on contention, preventing
reciprocal local sources from forming a cross-repository lock cycle. The source lease is released
before object transfer. A crashed source whose config is absent fails as a source instead of granting
the caller recovery authority over an independent repository.
Linked-worktree add, list, remove, move, and prune likewise record the invoking repository identity
before setup serialization. They revalidate after the wait and again at the path-based linked-worktree
library boundary, so plans derived before contention cannot be applied to a replacement repository.
This physical before-image restoration does not advance the intent; only `deinit` may resume the
semantic config transition, while status remains usable and init/update continue to report recovery
guidance.
Windows intent phase changes similarly move `intent.json` to the fixed `intent.previous` name and
flush it before installing and flushing the successor. A lone predecessor is restored as the active
intent; when both names survive, recovery verifies the predecessor belongs to the same transaction
and does not have a later phase before using the successor or performing any other recovery work.
Optional ownership and publication proofs may be introduced from `None` to `Some` at their journaled
phase, but every proof already present in the predecessor must remain exactly equal in the successor.
Every intent read revalidates that the active name still identifies the opened inode after the bytes
are parsed. Windows revalidates both surviving names after reading both, and predecessor promotion
revalidates the installed active name. Concurrent setup readers that both observe a lone predecessor
converge after one wins promotion only when the active name contains that exact inode and intent and
the predecessor name is absent; the losing reader also repeats the directory durability barrier.
Config restoration and mount recovery repeat the active-name check immediately before their first side effect.
The opened module repository
capability is retained through normal completion, and recovery reopens and verifies the recorded
repository identity and format before retirement, each config publication, and intent clearing.

After the final cleanliness proof, completion atomically moves the whole displaced checkout under a
private name inside the retained module repository. If the worktree and Git directory are on
different filesystems, it atomically retires the checkout under a recorded private sibling instead.
For a cross-directory retirement, the destination parent is flushed before the source parent; both
barriers complete before the journal records `Retired`. Recovery repeats the barriers when it finds
an already-retired checkout, so an interruption can at worst retain two durable names rather than
remove the checkout's only reachable name.
Recursive deletion is never used: a writer holding an old directory or file descriptor therefore
continues writing into the retired tree rather than losing data. Retired checkouts are deliberately
not garbage-collected automatically. Completion then unsets only the expected module
`core.worktree`, removes the writable local `[submodule "name"]` subsection, and retires the journal.
The recorded public empty-directory identity and emptiness are revalidated before config reservation
and again immediately before journal retirement. Both checks reopen every public-path component from
the current worktree root rather than trusting a retained parent capability, compare the opened
directory's identity with the record, inspect emptiness through that handle, and revalidate its
public name afterward. Concurrent public content or an ancestor or leaf replacement therefore leaves
a recoverable intent instead of producing a false successful deinit or an unusable unregistered
mount.
Terminal clearing validates the active intent identity and atomically retires its complete control
directory, so there is no state with a removed intent beneath an active recovery name. The empty
public mount and `modules/<name>` repository remain, allowing `update --init` to reattach without
recloning.

If a post-displacement proof discovers new local modifications, deinit journals rollback before restoring
the original mount. Rollback verifies that the public directory is still the recorded empty inode
and contains no concurrent content before moving it to the recorded rollback name. It then restores
the displaced checkout with a second no-replace rename. After restoring the checkout,
it journals that state, removes only the exact recorded empty private directory, journals cleanup,
reopens and validates the public mount from the current worktree root, and then retires the intent.
Recovery accepts crashes on either side of the empty-directory removal; unexpected content or
identity changes retain the intent and recovery guidance. A recovered rollback still reports the
original local-modification refusal. `RollingBack`, `RolledBack`, and `RollbackCleaned` retries use
the persisted intent's force and phase semantics; a new request without `--force` never repeats a
fresh cleanliness check against the restored dirty checkout before retiring the journal.

`init`, `update`, and `deinit` share the per-worktree mutation lock. Operations that may change the
shared superproject config also share the common config lock described above. Config publication and
before-image restoration pass an opaque lease into their blocking namespace workers, so cancelling
the awaiting future cannot release serialization while a worker can still move the active config.
Init reloads the effective
superproject configuration after acquiring that lock, and `update --init` uses the same locked init
path. Only the worktree that owns a deinit intent may complete it; shared-config mutations in sibling
worktrees return recovery guidance without changing the recorded config target. Conversely, deinit
refuses while update recovery owns its staging namespace.
`status` remains read-only and may inspect the repository while recovery is pending.

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
