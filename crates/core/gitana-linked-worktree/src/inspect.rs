//! Read-only inspection of one destination against one explicitly-identified repository.
//!
//! [`inspect`] never prunes, repairs, removes, or rewrites observed state — it reports. Every field the
//! requirements doc's "Inspection Requirements" enumerate is captured on [`WorktreeInspection`], from
//! which the pure [`classify`](crate::classify) function derives the partial-state classification.

use std::path::PathBuf;

use crate::WorktreeObjectId;
use crate::facts::{HeadKind, LockState};
use crate::query::BranchName;

/// What sits at the destination path on disk. A symlink is never followed, and a non-directory is never a
/// completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationKind {
	/// Nothing exists at the path.
	Absent,
	/// An empty directory.
	EmptyDir,
	/// A directory whose `.git` is a gitfile (`gitdir: <admin>`) — a linked-worktree checkout.
	LinkedWorktreeCheckout,
	/// The **destination itself** is a file, symlink, or other non-directory (never replaced). A directory
	/// whose *inner* `.git` is a symlink is not this — it is `UnrelatedContent` (the symlink is unfollowed).
	OtherFsObject,
	/// A non-empty directory with no valid linked-worktree `.git` gitfile — including one whose `.git` is a
	/// symlink (which is never followed).
	UnrelatedContent,
}

/// Whether the repository registers a linked worktree for this destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registration {
	/// No admin entry names this destination.
	None,
	/// An admin entry names this destination, and the checkout is present.
	Present {
		/// The admin directory `<common>/worktrees/<name>`.
		admin_dir: PathBuf,
	},
	/// An admin entry names this destination, but the checkout path is absent (git's "prunable").
	PresentCheckoutMissing {
		/// The admin directory whose checkout has gone.
		admin_dir: PathBuf,
	},
}

/// Health of the two cross-pointers between a checkout's `.git` gitfile and its admin `gitdir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossPointerHealth {
	/// No checkout gitfile to cross-check (not a linked-worktree checkout).
	NotApplicable,
	/// The checkout's `.git` names the admin **and** the admin's `gitdir` names the checkout's `.git`.
	Consistent,
	/// One or both pointers disagree (an identity/integrity conflict). The resolved targets are reported
	/// for diagnostics; neither is rewritten.
	Inconsistent {
		/// The admin directory the checkout's `.git` claims (resolved), if readable.
		checkout_points_to: Option<PathBuf>,
		/// The checkout `.git` file the admin's `gitdir` claims (resolved), if readable.
		admin_points_to: Option<PathBuf>,
	},
}

/// This destination's `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadFacts {
	/// Whether HEAD is symbolic, detached, or unborn.
	pub state: HeadKind,
	/// The branch ref name (`refs/heads/...`) when HEAD is symbolic; `None` when detached.
	pub branch: Option<String>,
	/// The resolved HEAD object; `None` when unborn.
	pub object: Option<WorktreeObjectId>,
}

/// Facts about the branch the caller asked for (`expected_branch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestedBranch {
	/// The query carried no `expected_branch`.
	NotRequested,
	/// The requested branch ref does not exist — but it may still be checked out **unborn** in another
	/// worktree (`git worktree add --orphan`), which is a branch-use conflict, so its checkout is reported.
	Absent {
		/// The checkout of *another* worktree parked on this (unborn) branch, if any.
		checked_out_elsewhere: Option<PathBuf>,
	},
	/// The requested branch exists.
	Exists {
		/// Its current object.
		object: WorktreeObjectId,
		/// The checkout of *another* worktree carrying this branch, if any (a branch-use conflict).
		checked_out_elsewhere: Option<PathBuf>,
	},
}

/// How the requested `start` commit relates to the worktree's current `HEAD` object — computed by
/// [`inspect`] (a reachability walk), so [`classify`](crate::classify) can distinguish a fast-forward from
/// a rewind/divergence without any I/O. Only present when both a `start` and a resolved `HEAD` object are
/// known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRelation {
	/// The worktree's `HEAD` object *is* the requested start — an exact match.
	Equal,
	/// The requested start is a proper ancestor of the worktree's `HEAD` — the branch advanced (a
	/// fast-forward); its current object is reported, never reset.
	Ancestor,
	/// The requested start is neither the `HEAD` object nor an ancestor of it — the branch was rewound or
	/// moved onto divergent history. A conflict: not a safe "advanced" match.
	Diverged,
}

/// A concrete disagreement with the requested identity (a start-point *divergence* is folded in via the
/// [`StartRelation`] on the inspection, computed by [`inspect`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityConflict {
	/// The checkout/admin cross-pointers disagree.
	CrossPointerDisagree,
	/// The registered worktree's HEAD is a different branch than requested (compared by *terminal* ref, so
	/// a symbolic alias matches its target). `found` is the worktree's terminal branch, or `None` when its
	/// HEAD is detached.
	RegisteredToDifferentBranch {
		/// The terminal branch the registered worktree is on, or `None` when detached.
		found: Option<String>,
	},
	/// The requested branch exists at an object other than the requested start (filled by `classify`).
	BranchAtUnexpectedObject {
		/// The branch's current object.
		found: WorktreeObjectId,
	},
	/// The destination is a checkout belonging to a different repository or worktree registration.
	DestinationBelongsToOtherWorktree {
		/// The admin directory the destination's `.git` claims.
		admin_dir: PathBuf,
	},
	/// More than one admin registration names this destination (corruption or a lost race).
	DuplicateRegistration {
		/// The admin directories that all claim this destination.
		admins: Vec<PathBuf>,
	},
}

/// A read-only snapshot of one destination against one repository. Nothing here mutates state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInspection {
	/// The inspected destination (echoed so a result is never applied to a replaced path).
	pub destination: PathBuf,
	/// The branch the query expected (echoed).
	pub expected_branch: Option<BranchName>,
	/// What sits at the destination on disk.
	pub destination_kind: DestinationKind,
	/// The repository's registration for this destination.
	pub registration: Registration,
	/// Cross-pointer health between checkout and admin.
	pub cross_pointers: CrossPointerHealth,
	/// This worktree's admin git directory (`<common>/worktrees/<name>`), when registered.
	pub git_dir: Option<PathBuf>,
	/// The repository's shared common dir (always known).
	pub common_dir: PathBuf,
	/// This destination's HEAD, when a HEAD is readable.
	pub head: Option<HeadFacts>,
	/// Facts about the requested branch.
	pub requested_branch: RequestedBranch,
	/// The requested start commit (echoed).
	pub start: Option<WorktreeObjectId>,
	/// How `start` relates to the worktree's current `HEAD` object, when both are known — the ancestry a
	/// fast-forward-vs-divergence decision needs, precomputed so `classify` stays pure.
	pub start_relation: Option<StartRelation>,
	/// The registration's lock state.
	pub lock: LockState,
	/// A start-independent identity conflict, if any.
	pub identity_conflict: Option<IdentityConflict>,
	/// The live checkout's working-tree status — populated only when the query set
	/// [`with_status`](crate::WorktreeQuery::with_status) **and** the destination is a present,
	/// cross-pointer-consistent linked-worktree checkout (a missing/partial/foreign checkout has no status to
	/// compute). `None` otherwise. A status *failure* is a hard error from `inspect`, never a silent `None`
	/// (which `classify` would read as clean).
	pub status: Option<crate::WorktreeStatusReport>,
	/// The live checkout's **residual** (untracked *or* ignored) working-tree paths — files on disk with no
	/// index entry, from a matcher-independent scan (see
	/// [`residual_untracked_paths`](crate::status)) — computed under the same condition as
	/// [`status`](WorktreeInspection::status), capped at a sample. An empty `Some(vec)` means the working tree
	/// is pristine (solely tracked files); a non-empty one is content safe removal must preserve (never
	/// delete), the conservative gate that keeps a non-git-faithful `.gitignore` match from ever authorising
	/// deletion of a git-untracked file. `None` when no status was requested / not a live checkout.
	pub residual_paths: Option<Vec<String>>,
	/// The live checkout's stage-0 tracked paths present on disk whose content or mode **diverges from the
	/// index**, verified by hashing the working file rather than trusting the index stat cache. Catches edits
	/// `status` can miss (a stat-preserving/same-size rewrite, a coarse-timestamp filesystem) and skip-worktree
	/// edits `status` omits entirely. Computed under the same condition as [`status`](WorktreeInspection::status).
	/// An empty `Some(vec)` means every present tracked file hashes equal to the index (reconstructable); a
	/// non-empty one is tracked-file content safe removal must preserve. `None` when no status was requested /
	/// not a live checkout.
	pub diverged_tracked_content: Option<Vec<String>>,
	/// Whether the live checkout uses a **sparse index** (`git sparse-checkout --sparse-index`) — its index
	/// carries a collapsed `040000` sparse-directory entry gitana does not expand, so a status computed over it
	/// reports spurious add/delete pairs. Computed under the same condition as
	/// [`status`](WorktreeInspection::status). `Some(true)` lets a removal decision refuse *honestly* (a clear
	/// "sparse-index unsupported" signal) instead of acting on that bogus status; `None` when no status was
	/// requested / not a live checkout.
	pub sparse_index: Option<bool>,
	/// The first **admin-local anchored commit** that removal would orphan, or `None` if every such commit is
	/// preserved. Removing a worktree deletes its admin dir and every reference living there — a detached (or
	/// per-worktree-symbolic) `HEAD`, and the `refs/worktree|bisect|rewritten/*` tips — each of which uniquely
	/// anchors its commit; a commit reachable from no *surviving* shared ref would be orphaned (later gc-able), so
	/// `Some(commit)` makes a removal decision refuse. A HEAD anchored by a surviving `refs/heads/*` branch is not
	/// at risk. `None` when no status was requested, there is no registered admin to remove, or every anchor is
	/// reachable. Computed for a live checkout **and** a checkout-missing partial.
	pub unreachable_admin_anchor: Option<WorktreeObjectId>,
	/// For a **checkout-missing partial** (a `PresentCheckoutMissing` registration), whether its retained index
	/// holds staged (index-vs-`HEAD`) or unmerged work. Cleaning the partial drops that index, so `Some(true)`
	/// means a removal decision refuses (the staged state would be lost). Computed only for a partial under a
	/// removal decision (`with_status`); a live checkout's staged changes are already reflected in
	/// [`status`](WorktreeInspection::status). `None` otherwise.
	pub partial_staged_changes: Option<bool>,
}

/// Classify what sits at `destination` without following a `.git` symlink or touching contents.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn classify_destination(
	destination: &std::path::Path,
) -> Result<DestinationKind, crate::LinkedWorktreeError> {
	use crate::LinkedWorktreeError;
	// `symlink_metadata` does not follow a symlink at the destination itself — but a **trailing separator**
	// (`.../wt-link/`) forces POSIX to resolve the leaf symlink, which would misclassify a symlinked destination
	// as its target's directory (and a later canonical delete would then destroy the real target). Stat the
	// trailing-separator-stripped leaf so a leaf symlink is always seen and classified `OtherFsObject`.
	let leaf = destination.components().as_path();
	let meta = match std::fs::symlink_metadata(leaf) {
		Ok(meta) => meta,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DestinationKind::Absent),
		Err(e) => return Err(LinkedWorktreeError::io("stat destination", destination, e)),
	};
	if !meta.is_dir() {
		// A file, symlink, fifo, ... — never replaced.
		return Ok(DestinationKind::OtherFsObject);
	}
	let mut entries = match std::fs::read_dir(destination) {
		Ok(entries) => entries,
		Err(e) => {
			return Err(LinkedWorktreeError::io(
				"reading destination",
				destination,
				e,
			));
		}
	};
	match entries.next() {
		None => return Ok(DestinationKind::EmptyDir),
		// A read error on the first entry is a *failure*, never silently "non-empty" — the failure-vs-
		// observation contract requires it be surfaced, not classified as content.
		Some(Err(e)) => {
			return Err(LinkedWorktreeError::io(
				"reading destination",
				destination,
				e,
			));
		}
		Some(Ok(_)) => {} // a real entry — the directory is non-empty
	}
	// A linked-worktree checkout's `.git` is a regular-file gitfile (`gitfile_target` returns `None` for a
	// `.git` directory or symlink, and errors on a malformed regular gitfile — git rejects those).
	let git = destination.join(".git");
	Ok(if crate::pointers::gitfile_target(&git)?.is_some() {
		DestinationKind::LinkedWorktreeCheckout
	} else {
		DestinationKind::UnrelatedContent
	})
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use super::*;

	use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
	use gitana_object_store::ObjectStore;
	use gitana_repository::{HeadState, Repository};

	use crate::LinkedWorktreeError;
	use crate::head::{read_head, read_lock_reason, structural_head_branch};
	use crate::object_id::IntoWorktreeObjectId;
	use crate::pointers::{
		RefSource, SYMREF_MAXDEPTH, admin_dirs_for, admin_gitdir_target, admin_owned_by,
		branch_checkout_location, canonical_eq, checkout_gitfile_names, gitfile_target,
		resolve_ref_terminal,
	};
	use crate::query::WorktreeQuery;
	use crate::repo_id::{detect_kind, open_store_raw};

	/// Inspect one destination. Opens the repository store (native mint), detects the object format, and
	/// dispatches to the monomorphized body.
	pub async fn inspect(query: &WorktreeQuery) -> Result<WorktreeInspection, LinkedWorktreeError> {
		inspect_impl(query, true).await
	}

	/// Like [`inspect`] but reads HEAD **structurally** — the branch HEAD directly names, without following the
	/// symref chain to a terminal branch or resolving it to an object ([`HeadFacts::object`] is `None`,
	/// [`HeadFacts::branch`] is the unpeeled ref). This is what `git worktree move` needs: it validates HEAD's
	/// structure and matches its (unpeeled) branch, but never resolves the chain, so a worktree whose HEAD
	/// chain is cyclic or unreadable is still classifiable and movable. Kept **internal** so the public
	/// [`WorktreeQuery`] construction API is unchanged — the mode is a `relocate` concern, not a query field.
	pub(crate) async fn inspect_structural_head(
		query: &WorktreeQuery,
	) -> Result<WorktreeInspection, LinkedWorktreeError> {
		inspect_impl(query, false).await
	}

	/// The shared inspection. `resolve_head` selects the resolving (default, via [`inspect`]) or structural
	/// (via [`inspect_structural_head`]) HEAD read; it also governs whether a pinned `expected_branch` is
	/// peeled to its terminal before the identity compare — in structural mode neither side is peeled.
	async fn inspect_impl(
		query: &WorktreeQuery,
		resolve_head: bool,
	) -> Result<WorktreeInspection, LinkedWorktreeError> {
		// Identity paths must be absolute — a relative destination would resolve against the process CWD.
		if !query.destination.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(query.destination.clone()));
		}
		// The top-level store reads *shared* state (object format, refs, ancestry) — anchor it on the stable
		// `common_dir`, never the identity's `git_dir` (which, for an identity discovered inside a linked
		// worktree, names that checkout's admin and would fail to open once the checkout is pruned). A
		// specific worktree's per-worktree state is opened against its own admin where it is read.
		let common = query.repo.common_dir();
		let store = open_store_raw(common, common)?;
		let kind = detect_kind(&store).await?;
		// A `start` of the wrong hash format cannot belong to (or be created in) this repository — reject it
		// up front, rather than silently dropping the ancestry relation and, e.g., reporting AbsentSafeToCreate.
		if let Some(start) = &query.start
			&& start.kind() != kind
		{
			return Err(LinkedWorktreeError::InvalidObjectId {
				kind,
				hex: start.to_hex(),
			});
		}
		match kind {
			HashKind::Sha1 => {
				inspect_generic::<Sha1>(
					Repository::new(ObjectStore::new(store)),
					query,
					resolve_head,
				)
				.await
			}
			HashKind::Sha256 => {
				inspect_generic::<Sha256>(
					Repository::new(ObjectStore::new(store)),
					query,
					resolve_head,
				)
				.await
			}
		}
	}

	async fn inspect_generic<H: HashAlgorithm>(
		repo: Repository<gitana_file_store_local::WorktreeFileStore, H>,
		query: &WorktreeQuery,
		resolve_head: bool,
	) -> Result<WorktreeInspection, LinkedWorktreeError>
	where
		ObjectId<H>: IntoWorktreeObjectId,
	{
		let common = query.repo.common_dir();
		let destination = &query.destination;
		let destination_kind = classify_destination(destination)?;

		// Registration: the admin(s) whose recorded checkout is this destination. More than one is
		// corruption (a duplicate registration) — surfaced as an identity conflict, never silently taking
		// the first. Presence is judged by the checkout's `.git` gitfile *identity*, not by the directory
		// merely existing — a checkout replaced by an empty directory/symlink is a missing checkout (git's
		// "prunable"), not a live one.
		let registered_admins = admin_dirs_for(common, destination)?;
		let duplicate_registration = (registered_admins.len() > 1).then(|| registered_admins.clone());
		let registered_admin = match registered_admins.as_slice() {
			[admin] => Some(admin.clone()),
			_ => None,
		};
		let registration = match &registered_admin {
			Some(admin) if checkout_gitfile_names(destination, admin)? => Registration::Present {
				admin_dir: admin.clone(),
			},
			Some(admin) => Registration::PresentCheckoutMissing {
				admin_dir: admin.clone(),
			},
			None => Registration::None,
		};

		// The admin the checkout's own `.git` claims (checkout→admin direction). A claim to an admin
		// *outside* this repository is foreign — recorded as a conflict and **never dereferenced** (its
		// HEAD/lock/`gitdir` are not read), so an untrusted destination cannot trigger ambient reads.
		let gitfile = destination.join(".git");
		let claimed_admin = if destination_kind == DestinationKind::LinkedWorktreeCheckout {
			gitfile_target(&gitfile)?
		} else {
			None
		};
		// Ownership requires both physical position under `worktrees/` *and* a `commondir` naming this
		// repository — an admin under our `worktrees/` whose `commondir` targets another repository is
		// foreign, and its HEAD/lock must never be read against us (that would fabricate facts).
		let (owned_admin, foreign_admin) = match &claimed_admin {
			Some(admin) if admin_owned_by(common, admin)? => (Some(admin.clone()), None),
			Some(admin) => (None, Some(admin.clone())),
			None => (None, None),
		};

		// Cross-pointers are only computed for an admin under this repository.
		let cross_pointers = match &owned_admin {
			None => CrossPointerHealth::NotApplicable,
			Some(admin) => {
				let admin_back = admin_gitdir_target(admin);
				let consistent = admin_back
					.as_deref()
					.is_some_and(|back| canonical_eq(back, &gitfile));
				if consistent {
					CrossPointerHealth::Consistent
				} else {
					CrossPointerHealth::Inconsistent {
						checkout_points_to: owned_admin.clone(),
						admin_points_to: admin_back,
					}
				}
			}
		};

		// The admin dir holding this destination's HEAD/lock — the registration's, else an *owned* claimed
		// one. Never a foreign admin.
		let head_dir = registered_admin.clone().or_else(|| owned_admin.clone());

		let head = match &head_dir {
			None => None,
			// Structural (no-resolve) HEAD, as `git worktree move` reads it: name the branch HEAD *directly*
			// carries without following the symref chain or touching the object store, so a cyclic/unreadable
			// chain still yields a movable worktree. `object` is unknown here (`None`); `branch` is the unpeeled
			// ref. A structurally invalid HEAD (absent/malformed) is `None`, which the move refuses as invalid.
			Some(dir) if !resolve_head => match structural_head_branch(dir) {
				None => None,
				Some(Some(branch)) => Some(HeadFacts {
					state: HeadKind::Symbolic,
					branch: Some(branch),
					object: None,
				}),
				Some(None) => Some(HeadFacts {
					state: HeadKind::Detached,
					branch: None,
					object: None,
				}),
			},
			Some(dir) => match read_head::<H>(dir)? {
				None => None,
				Some(HeadState::Symbolic(refname)) => {
					// Report the *terminal* branch (`HEAD` → `alias` → `feature` is "on feature", as git's
					// worktree list shows). Resolve the object through a store scoped to *this* worktree's admin
					// dir, so a per-worktree ref target is read from the right namespace; `resolve_symbolic`
					// follows the same chain to the object.
					// `refname` is `HEAD`'s target (`HEAD` already read here), so one hop of git's budget is spent.
					let terminal =
						resolve_ref_terminal(common, dir, &refname, RefSource::Head, SYMREF_MAXDEPTH - 1)?;
					let store = Repository::<_, H>::new(ObjectStore::new(open_store_raw(dir, common)?));
					// Resolve the object through the *terminal* ref, not the original `HEAD` target: a legacy
					// *symlink* symref (`refs/heads/alias -> refs/heads/feature`) is symbolic to git, but the
					// filesystem backend following that link relative to `refs/heads` would miss the object.
					let object = store
						.refs()
						.resolve_symbolic(&terminal)
						.await?
						.map(IntoWorktreeObjectId::tag);
					Some(HeadFacts {
						state: if object.is_some() {
							HeadKind::Symbolic
						} else {
							HeadKind::Unborn
						},
						branch: Some(terminal),
						object,
					})
				}
				Some(HeadState::Detached(id)) => Some(HeadFacts {
					state: HeadKind::Detached,
					branch: None,
					object: Some(id.tag()),
				}),
			},
		};

		let requested_branch = match &query.expected_branch {
			None => RequestedBranch::NotRequested,
			Some(branch) => {
				let refname = branch.refname();
				// Resolve the requested branch's *terminal* ref FIRST — before the occupancy scan below — a
				// legacy *symlink* symref (`refs/heads/alias -> refs/heads/feature`) is symbolic to git, but
				// the file-store backend following it filesystem-relative from `refs/heads` would miss it and
				// wrongly report Absent. A direct ref (not reached via `HEAD`) gets the full symref budget; a
				// malformed *requested* name is a bad argument (`InvalidRequestedBranch`), and a *valid* branch
				// whose on-disk symref chain is corrupt is `MalformedPointer` of kind `Ref` rooted at the
				// branch — **neither blamed on `HEAD`**. This must run before `branch_checkout_location`: if the
				// requested branch is *also* checked out, that scan would otherwise peel the occupying
				// worktree's `HEAD` onto the same corrupt chain and surface it as a `Head` malformation first,
				// hiding the branch-rooted classification the requester actually wants.
				let terminal = resolve_ref_terminal(
					common,
					common,
					&refname,
					RefSource::RequestedBranch(branch.short()),
					SYMREF_MAXDEPTH,
				)?;
				// Then scan for another worktree on this branch *regardless of whether the ref exists* — an
				// unborn branch (`worktree add --orphan`) is checked out with no ref, and creating it would
				// collide, so an occupied unborn branch is a conflict too. Skip *this* destination (a
				// `worktree add --force` duplicate is still a conflict). The scan compares the *raw* (unpeeled)
				// ref name, as git's shared-symref test does.
				let elsewhere = branch_checkout_location(common, &refname, Some(destination))?;
				match repo.refs().resolve_symbolic(&terminal).await? {
					None => RequestedBranch::Absent {
						checked_out_elsewhere: elsewhere,
					},
					Some(id) => RequestedBranch::Exists {
						object: id.tag(),
						checked_out_elsewhere: elsewhere,
					},
				}
			}
		};

		// How the requested `start` relates to the worktree's current object — a reachability walk done here
		// (read-only) so `classify` need do no I/O. `Equal` (exact), `Ancestor` (a fast-forward — the branch
		// advanced), or `Diverged` (rewound / unrelated history — a conflict, never a safe "advanced"). Only
		// when both a start and a resolved HEAD object are known and share this repository's hash format.
		let start_relation = match (&query.start, head.as_ref().and_then(|h| h.object.as_ref())) {
			(Some(start), Some(head_object)) => {
				match (
					ObjectId::<H>::from_hex(&start.to_hex()),
					ObjectId::<H>::from_hex(&head_object.to_hex()),
				) {
					(Ok(start_h), Ok(head_h)) if start_h == head_h => Some(StartRelation::Equal),
					(Ok(start_h), Ok(head_h)) => Some(if repo.is_ancestor(start_h, head_h).await? {
						StartRelation::Ancestor
					} else {
						StartRelation::Diverged
					}),
					// A start of a different hash format than the repository can bear no ancestry relation.
					_ => None,
				}
			}
			_ => None,
		};

		let lock = head_dir
			.as_deref()
			.map(read_lock_reason)
			.unwrap_or(LockState::Unlocked);

		// Compare branch identity by *terminal* ref (a symbolic alias matches its target), so an exact
		// retry of an `alias`-created worktree is not a false conflict. In **structural** mode neither side is
		// peeled: the worktree branch above is the unpeeled ref HEAD names, so the expected pin must stay
		// unpeeled too — peeling only one side would report a genuine match as `RegisteredToDifferentBranch`.
		let registered = !matches!(registration, Registration::None);
		let registered_to_different_branch = match (&query.expected_branch, registered) {
			(Some(expected), true) => {
				let expected_terminal = if resolve_head {
					// A *direct* ref (the requested branch, not reached via `HEAD`) — the full symref budget applies.
					resolve_ref_terminal(
						common,
						common,
						&expected.refname(),
						RefSource::RequestedBranch(expected.short()),
						SYMREF_MAXDEPTH,
					)?
				} else {
					expected.refname()
				};
				let worktree_terminal = head.as_ref().and_then(|h| h.branch.clone());
				(worktree_terminal.as_deref() != Some(expected_terminal.as_str()))
					.then_some(worktree_terminal)
			}
			_ => None,
		};

		// A duplicate registration (corruption) outranks all; then a foreign claim (found before any
		// dereference); then a cross-pointer disagreement; then a registered-to-a-different-branch mismatch.
		let identity_conflict = if let Some(admins) = duplicate_registration {
			Some(IdentityConflict::DuplicateRegistration { admins })
		} else if let Some(admin) = foreign_admin {
			Some(IdentityConflict::DestinationBelongsToOtherWorktree { admin_dir: admin })
		} else if matches!(cross_pointers, CrossPointerHealth::Inconsistent { .. }) {
			Some(IdentityConflict::CrossPointerDisagree)
		} else {
			registered_to_different_branch
				.map(|found| IdentityConflict::RegisteredToDifferentBranch { found })
		};

		// Working-tree status — computed only when the caller asked for it (a removal/cleanup decision) **and**
		// the destination is a present, cross-pointer-consistent live checkout: a missing/partial checkout has
		// no working tree to scan, and a foreign one must never be opened. A status *failure* propagates as a
		// hard error (never a silent `None`, which `classify` would read as clean).
		let statusable = query.with_status
			&& matches!(registration, Registration::Present { .. })
			&& cross_pointers == CrossPointerHealth::Consistent
			&& destination_kind == DestinationKind::LinkedWorktreeCheckout;
		let (status, residual_paths, diverged_tracked_content, sparse_index) = if statusable {
			let admin = registered_admin
				.as_ref()
				.expect("a Present registration carries its admin dir");
			// All computed for a removal decision: the three-way status (tracked-side cleanliness), a
			// matcher-independent scan for residual untracked/ignored content (the conservative delete gate), a
			// content re-verification of every present tracked file (hashing, not the stat cache — catches edits
			// `status` can miss and skip-worktree edits it omits), and whether the index is a sparse index (whose
			// collapsed directory entries make that status unreliable).
			let status = crate::status::status_at(admin, common, destination).await?;
			let residual = crate::status::residual_untracked_paths(admin, common, destination).await?;
			let diverged =
				crate::status::diverged_tracked_content_paths(admin, common, destination).await?;
			let sparse = crate::status::is_sparse_index(admin, common, destination).await?;
			(Some(status), Some(residual), Some(diverged), Some(sparse))
		} else {
			(None, None, None, None)
		};

		// Commit-preservation: for a removal decision (`with_status`) on any *registered* worktree — live **or**
		// a checkout-missing partial, since both delete the admin dir — every commit anchored *only* by something
		// inside that admin (a detached/per-worktree-symbolic `HEAD`, and the `refs/worktree|bisect|rewritten/*`
		// tips) must be reachable from a surviving shared ref, else removal orphans it. `first_unreachable_admin_anchor`
		// returns the first such orphaned commit (or `None`). The `HEAD` commit is handed to it **only** when its
		// own anchor will not survive: a HEAD symbolic to a shared `refs/heads/*` branch keeps its commit reachable
		// via that surviving branch, so it is not passed (but the per-worktree ref tips are always checked).
		let unreachable_admin_anchor = if query.with_status
			&& let Some(admin) = registered_admin.as_ref()
		{
			let head_at_risk = head.as_ref().and_then(|h| {
				let anchored_by_shared_branch = h
					.branch
					.as_deref()
					.is_some_and(|b| b.starts_with("refs/heads/"));
				h.object.as_ref().filter(|_| !anchored_by_shared_branch)
			});
			crate::status::first_unreachable_admin_anchor(admin, common, head_at_risk).await?
		} else {
			None
		};

		// A checkout-missing partial keeps its per-worktree index; if that index has staged/unmerged work,
		// cleaning the partial (dropping the admin) would erase it. Computed only for that case — a live
		// checkout's staged changes are already covered by `status.has_tracked_changes()`.
		let partial_staged_changes = if query.with_status
			&& let Registration::PresentCheckoutMissing { admin_dir } = &registration
		{
			Some(crate::status::partial_has_staged_changes(admin_dir, common).await?)
		} else {
			None
		};

		Ok(WorktreeInspection {
			destination: destination.clone(),
			expected_branch: query.expected_branch.clone(),
			destination_kind,
			registration,
			cross_pointers,
			git_dir: registered_admin,
			common_dir: common.to_path_buf(),
			head,
			requested_branch,
			start: query.start.clone(),
			start_relation,
			lock,
			identity_conflict,
			status,
			residual_paths,
			diverged_tracked_content,
			sparse_index,
			unreachable_admin_anchor,
			partial_staged_changes,
		})
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::inspect;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::inspect_structural_head;
