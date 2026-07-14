# Code Henge Linked-Worktree Requirements

## Status and Audience

This document defines requirements for the Gitana functionality needed by Code
Henge to manage persistent editable workspaces. It is written for the Gitana
agent responsible for satisfying those requirements.

This is a requirements document, not a design or implementation plan. It does
not prescribe crate boundaries, internal types, algorithms, storage layouts, or
how existing Gitana code should be reorganized.

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are
normative.

## Consumer Context

Code Henge represents one logical workspace as a set of repository mappings.
Read-only mappings continue to use a project's primary worktree. Writable
mappings use Git linked worktrees at Code Henge-selected paths.

Code Henge persists its own workspace intent before asking Gitana to perform a
Git or filesystem effect. A process may stop at any point during linked-worktree
creation or removal. After restart, Code Henge must be able to inspect the
repository and destination, distinguish safe completion from a conflict, and
continue without overwriting user data.

One Code Henge workspace may span multiple independent repositories. Gitana is
responsible only for one repository and one linked worktree at a time. Code
Henge remains responsible for multi-repository ordering, persistence, retry,
and aggregate lifecycle.

## Required Scope

Gitana MUST provide an in-process Rust library surface for all behavior in this
document. The required behavior MUST NOT require invoking a Gitana or Git
command-line process.

The surface MUST support:

- validation of an explicit linked-worktree request;
- inspection of existing linked-worktree and branch state;
- creation or safe completion of a requested linked worktree;
- structured reporting of partial, matching, and conflicting state;
- worktree cleanliness inspection; and
- safe, identity-checked linked-worktree removal.

Every operation that performs I/O MUST follow Gitana's async conventions.

## Repository and Object-Format Coverage

The required operations MUST work with:

- SHA-1 repositories;
- SHA-256 repositories;
- repositories with an ordinary primary worktree;
- bare repositories that may host linked worktrees;
- calls made while another linked worktree is the discovered repository; and
- separate per-worktree Git directories and shared common directories.

The caller MUST be able to identify the repository explicitly enough that
Gitana does not need to infer Code Henge project ownership from the destination
path.

## Explicit Creation Inputs

A linked-worktree creation request MUST allow the caller to provide, without
DWIM defaults:

- the repository to which the linked worktree belongs;
- the destination checkout path;
- the exact local branch name to be checked out;
- the exact starting commit, including its object format; and
- whether the branch is expected to be newly created or is being reconciled
  after an interrupted prior attempt.

Gitana MUST validate the branch name according to Git ref rules before making a
change.

Gitana MUST reject ambiguous requests rather than choosing a branch, deriving a
branch from the destination basename, changing the start point, or selecting a
detached checkout implicitly.

The request MUST NOT provide a force-reset mode for Code Henge's use. An
existing branch at an unexpected object, an existing checkout at an unexpected
path, or a destination with unrelated content MUST be reported as a conflict.

## Inspection Requirements

The caller MUST be able to inspect the state relevant to one requested linked
worktree without changing it.

Inspection MUST report enough structured information to determine:

- whether the destination path is absent, an empty directory, a linked
  worktree checkout, another kind of filesystem object, or a directory with
  unrelated content;
- whether the destination is registered in the repository's linked-worktree
  administration;
- whether a registration exists whose checkout is missing;
- whether the checkout's `.git` file and the administrative `gitdir` pointer
  identify each other correctly;
- the per-worktree Git directory and shared common directory;
- whether `HEAD` is symbolic, detached, or unborn;
- the symbolic branch name, when present;
- the current `HEAD` object, when present;
- whether the requested branch exists and its current object;
- whether the requested branch is checked out in another worktree;
- whether the worktree is locked and the lock reason, when available; and
- whether any observed path or registration conflicts with the requested
  repository, destination, or branch identity.

Inspection MUST distinguish a missing checkout with a retained registration
from a fully absent linked worktree. It MUST NOT silently prune, repair, remove,
or rewrite observed state.

The caller MUST also be able to enumerate linked worktrees for a repository in
a structured form. Enumeration MUST include the primary worktree, linked
worktrees, missing registered worktrees, detached worktrees, bare state, lock
state, branch identity, and current object where those values exist.

## Creation and Reconciliation Requirements

Gitana MUST support establishing the exact linked worktree described by an
explicit creation request.

On success, the resulting state MUST be Git-compatible and MUST include:

- a repository registration for the destination;
- mutually consistent administrative and checkout cross-pointers;
- the requested symbolic branch in `HEAD`;
- a worktree index appropriate for the requested starting commit;
- checkout contents for the requested starting commit; and
- the ref and reflog effects required by the repository's configured Git
  behavior.

The operation MUST return structured facts about the resulting linked
worktree. It MUST NOT rely on human-readable stdout or stderr as its result.

Repeated execution for an already-complete, exactly matching linked worktree
MUST be idempotent and MUST report that the requested state already exists.

When an earlier attempt created the requested branch at the requested starting
commit but did not complete the linked worktree, Gitana MUST allow the caller
to distinguish that recoverable state from a pre-existing branch conflict.

When the exact destination is already registered to the exact requested branch,
Gitana MUST report the observed linked worktree without resetting its branch or
discarding changes. A branch that advanced after the worktree was created MUST
not be reset to the original starting commit.

Gitana MUST refuse to complete creation automatically when any of the following
is true:

- the destination contains unrelated or unclassified content;
- the destination belongs to another repository or worktree registration;
- the requested branch is checked out at another destination;
- the requested branch exists at an unexpected object and is not already the
  exact requested destination's branch;
- cross-pointers identify different administrative or checkout paths;
- the relevant worktree registration is locked against the requested action;
  or
- completing the request would overwrite, delete, or relocate user data.

Each refusal MUST be distinguishable by structured outcome or error data.

## Partial-State and Recovery Requirements

Linked-worktree operations span multiple durable Git and filesystem effects.
Gitana MUST make every partial state produced by interruption observable on the
next call.

The caller MUST be able to distinguish at least these conditions:

| Observed condition | Required classification |
| --- | --- |
| No branch, registration, or destination | Absent and safe to create |
| Requested branch exists at the requested start; no registration or checkout | Interrupted state that may be completed safely |
| Registration and checkout both match the request | Complete and idempotent |
| Registration exists; checkout is missing | Partial registered state |
| Checkout exists; registration is missing or inconsistent | Partial conflicting state |
| Exact worktree exists and its requested branch has advanced | Matching worktree with current state preserved |
| Requested branch is checked out elsewhere | Branch-use conflict |
| Destination contains unrelated content | Destination conflict |
| Cross-pointers disagree | Identity or integrity conflict |
| Worktree is dirty, conflicted, or locked | Protected state with its reason reported |

Cancellation or task loss MUST NOT cause Gitana to report success before the
requested state is observable. A cancelled operation MAY leave a partial state,
but that state MUST remain inspectable and MUST be classifiable on retry.

Recovery behavior MUST be safe under repeated calls. Repetition MUST NOT reset
an advanced branch, delete unknown content, duplicate registrations, or create
multiple administrative entries for one destination.

## Status and Cleanliness Requirements

The caller MUST be able to obtain structured status for a linked worktree before
requesting cleanup.

Status MUST distinguish:

- clean worktrees;
- staged changes;
- unstaged tracked changes;
- untracked paths;
- conflicted paths;
- missing tracked paths; and
- failures that prevent status from being determined reliably.

Ignored paths SHOULD remain excluded according to Git-compatible status
semantics. A status failure MUST NOT be treated as a clean worktree.

The status result MUST be associated with the inspected linked-worktree
identity so callers can avoid applying a stale result to a replaced path.

## Removal Requirements

Removal MUST require the caller to identify the expected repository and linked
worktree destination. Gitana MUST verify that identity again immediately before
performing destructive effects.

The safe default removal behavior required by Code Henge MUST:

- remove only a linked worktree, never the repository's primary worktree;
- refuse a dirty or conflicted worktree;
- refuse a locked worktree;
- refuse a destination or registration identity mismatch;
- preserve untracked and unknown files;
- preserve the local branch and its commits;
- avoid deleting unrelated administrative entries; and
- be idempotent when the exact linked worktree is already absent.

Removal MUST return structured outcomes that distinguish successful removal,
already-absent state, dirty preservation, lock refusal, identity mismatch, and
an incomplete removal that requires later inspection.

The required Code Henge surface MUST NOT expose a mode that silently discards a
dirty worktree or deletes its branch. Other Gitana consumers may have separate
force behavior, but Code Henge must be able to use the safe behavior without
enabling it.

## Data-Preservation and Security Requirements

Operations MUST NOT:

- follow a destination `.git` symlink as if it were a valid linked-worktree
  file;
- replace a non-directory destination;
- remove or rewrite files that have not been identified as belonging to the
  exact requested linked worktree;
- delete a local branch or commit during linked-worktree removal;
- treat canonical-path equality as sufficient when Git cross-pointer identity
  disagrees;
- rewrite another worktree's `HEAD`, index, reflog, lock, or operation state;
  or
- weaken repository-format, ref-locking, compare-and-swap, or worktree branch
  exclusion guarantees already enforced by Gitana.

Paths MUST be accepted as native filesystem paths without requiring UTF-8
conversion. Human-readable diagnostics MAY use lossy display, but structured
identity and operation inputs MUST retain the native path.

Repository and branch changes MUST remain safe when another process changes a
ref or worktree registration concurrently. A lost race MUST be reported as a
conflict rather than overwriting the winner.

## Git Compatibility Requirements

For the supported request forms, observable repository state MUST remain
compatible with stock Git. This includes:

- branch ref validation and locking;
- refusal when a branch is checked out elsewhere;
- linked-worktree administrative naming and cross-pointers;
- per-worktree `HEAD`, index, reflog, and operation-state isolation;
- shared common-directory refs and objects;
- configured reflog behavior;
- SHA-1 and SHA-256 object IDs;
- ordinary and bare repositories; and
- discovery from either a primary or linked worktree.

Existing Gitana command behavior that consumes the same underlying operations
MUST retain its tested Git parity.

## Library-Consumer Requirements

The required behavior MUST be available to a Rust consumer through normal Cargo
dependency resolution at a pinned Git revision. It MUST NOT require a sibling
path dependency.

The public consumer surface MUST:

- return structured success, observation, refusal, and failure data;
- preserve underlying error chains for diagnostics;
- avoid writing command-oriented output directly to stdout or stderr;
- avoid process-global current-directory changes;
- make no assumptions about Code Henge's SQLite schema or lifecycle states;
  and
- permit Code Henge to supply its own persistence, retry, and logging policy.

## Validation Requirements

Automated validation MUST cover:

- clean creation in SHA-1 and SHA-256 repositories;
- creation from ordinary, bare, and linked-worktree discovery contexts;
- exact branch and starting-commit selection without DWIM behavior;
- branch-name rejection;
- branch checked out in another worktree;
- an existing branch at an unexpected object;
- destination collisions with files, non-empty directories, symlinks, and
  another repository's worktree;
- repeated creation of an exact matching worktree;
- recovery after each externally observable creation boundary;
- a missing checkout with retained registration;
- inconsistent `.git` and `gitdir` cross-pointers;
- an advanced branch in an otherwise matching linked worktree without reset or
  data loss;
- structured clean, dirty, untracked, and conflicted status;
- safe removal of a clean exact worktree;
- refusal to remove dirty, conflicted, locked, primary, or mismatched
  worktrees;
- repeated removal after the exact worktree is absent;
- preservation of branches, commits, unknown content, and unrelated worktree
  administration;
- concurrent ref or registration races; and
- parity with stock Git for the repository state and refusal cases above.

The Gitana workspace's standard formatting, test, check, lint, and relevant
cross-target validation MUST remain green.

## Acceptance Criteria

The requirements are satisfied when a Code Henge adapter can, without spawning
a command-line process:

1. submit an explicit repository, destination, branch, and starting commit;
2. establish the requested linked worktree or receive a structured conflict;
3. repeat the request safely after interruption;
4. inspect and classify every partial state listed in this document;
5. determine whether the exact linked worktree is clean, dirty, conflicted,
   locked, missing, or mismatched;
6. remove only a clean, exact linked worktree while retaining its branch and
   commits;
7. preserve user and unrelated repository data in every refusal and recovery
   case; and
8. consume the functionality from a reproducible pinned Git revision.

## Non-Requirements

Gitana is not required to implement or understand:

- Code Henge project, workspace, session, or operation identifiers;
- Code Henge's SQLite persistence or transaction boundaries;
- multi-repository workspace orchestration;
- session claims or capability grants;
- Code Henge retry schedules or readiness policy;
- Code Henge's default workspace or branch naming policy;
- branch integration, merge, rebase, publication, or deletion;
- workspace movement;
- automatic destructive cleanup of dirty worktrees; or
- a guarantee that a multi-effect linked-worktree operation is atomically
  crash-free.

Gitana is required to make partial effects observable and safely classifiable;
Code Henge remains responsible for durable intent and recovery policy.
