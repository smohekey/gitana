//! Establishing a linked worktree from an explicit [`CreateRequest`], reconciling against slice-1's
//! read-only inspection so a repeat is idempotent and a mismatched destination is refused.
//!
//! `create` **inspects first**, then decides *against the requested target*: an already-present worktree
//! that matches the request is a no-op; an absent destination is created; anything else — a present
//! worktree that differs, a conflict, a protected/partial state — is refused with a matchable
//! [`CreateError`]. The write side ports git's admin-layout + reflog + checkout materialisation, ordered
//! so the checkout's `.git` gitfile (the marker that makes a registration look *live*) is written **last**
//! — an interrupted create is therefore a `PartialRegistered` half-state, never a false "complete".

#[cfg(not(target_arch = "wasm32"))]
mod native {
	use std::path::{Path, PathBuf};
	use std::time::{SystemTime, UNIX_EPOCH};

	use gitana_config::GitConfig;
	use gitana_object::{HashAlgorithm, HashKind, ObjectId, Sha1, Sha256};
	use gitana_object_store::ObjectStore;
	use gitana_repository::{HeadState, RefOp, ReflogIntent, Repository};
	use gitana_worktree::{Index, WorkTree};

	use crate::create_error::CreateError;
	use crate::facts::{HeadKind, LockState};
	use crate::inspect::{
		CrossPointerHealth, DestinationKind, Registration, RequestedBranch, inspect,
	};
	use crate::object_id::IntoWorktreeObjectId;
	use crate::pointers::is_valid_refname;
	use crate::query::WorktreeQuery;
	use crate::repo_id::{detect_kind, open_store_raw, open_work_dir};
	use crate::request::{CheckoutTarget, CreateRequest};
	use crate::{LinkedWorktreeError, WorktreeClassification, WorktreeInspection, WorktreeObjectId};

	/// What the create should do, decided from the inspection against the requested target.
	enum Action {
		/// The requested worktree already exists exactly — a no-op.
		AlreadyThere,
		/// Write the worktree; `create_branch` is `true` only for a fresh `NewBranch` whose ref is absent.
		Write { create_branch: bool },
	}

	/// Establish the linked worktree described by `request`, reconciling against its current state.
	///
	/// `effective` is the merged config to honour for the committer identity (every reflog line) and
	/// `core.logAllRefUpdates` reflog gating — `None` uses the repository-local config alone. Returns the
	/// resulting [`WorktreeInspection`] on success (including an idempotent no-op when it already existed
	/// exactly); every refusal/failure is a [`CreateError`].
	pub async fn create(
		request: &CreateRequest,
		effective: Option<&GitConfig>,
	) -> Result<WorktreeInspection, CreateError> {
		if !request.destination.is_absolute() {
			return Err(LinkedWorktreeError::RelativePath(request.destination.clone()).into());
		}
		validate_target_name(&request.target)?;

		let query = request_query(request);
		let inspection = inspect(&query).await?;

		match decide(&inspection, &request.target)? {
			Action::AlreadyThere => return Ok(inspection),
			Action::Write { create_branch } => {
				let common = request.repo.common_dir();
				let store = open_store_raw(common, common)?;
				match detect_kind(&store).await? {
					HashKind::Sha1 => write_worktree::<Sha1>(request, effective, create_branch).await?,
					HashKind::Sha256 => write_worktree::<Sha256>(request, effective, create_branch).await?,
				}
			}
		}

		// Re-inspect so the caller receives the resulting, now-established state.
		Ok(inspect(&query).await?)
	}

	/// Decide the create action from the inspection *against the requested target*, or refuse.
	fn decide(
		inspection: &WorktreeInspection,
		target: &CheckoutTarget,
	) -> Result<Action, CreateError> {
		use WorktreeClassification as C;

		// A locked registration, an unrelated/non-directory destination, or a structural identity conflict
		// all block a create outright — the shared read-model refusals.
		if let LockState::Locked { reason } = &inspection.lock {
			return Err(CreateError::Refused(C::ProtectedWithReason {
				reason: crate::ProtectionReason::Locked {
					reason: reason.clone(),
				},
			}));
		}
		if matches!(
			inspection.destination_kind,
			DestinationKind::OtherFsObject | DestinationKind::UnrelatedContent
		) {
			return Err(CreateError::Refused(C::DestinationConflict {
				kind: inspection.destination_kind.clone(),
			}));
		}
		if let Some(detail) = &inspection.identity_conflict {
			return Err(CreateError::Refused(C::IdentityConflict {
				detail: detail.clone(),
			}));
		}

		// The branch the target names is checked out in *another* worktree — git refuses a second checkout.
		// Checked before the idempotent return, so a branch force-duplicated across worktrees is surfaced
		// rather than silently reported as already-present.
		if let Some(other) = requested_checked_out_elsewhere(&inspection.requested_branch) {
			return Err(CreateError::Refused(C::BranchUseConflict {
				other_checkout: other.to_path_buf(),
			}));
		}

		// A live worktree already sits at the destination: idempotent only when it *matches the request*
		// (kind, branch, and — for a branch/detached target — the start relation), otherwise it is a
		// different worktree occupying the destination.
		let live = matches!(inspection.registration, Registration::Present { .. })
			&& inspection.cross_pointers == CrossPointerHealth::Consistent;
		if live && inspection.head.is_some() {
			return if matches_target(inspection, target) {
				Ok(Action::AlreadyThere)
			} else {
				Err(CreateError::ExistingWorktreeMismatch(Box::new(
					inspection.clone(),
				)))
			};
		}

		// A retained registration whose checkout is gone (git's prunable) — the caller prunes and retries;
		// a create never silently repairs it.
		if let Registration::PresentCheckoutMissing { admin_dir } = &inspection.registration {
			return Err(CreateError::Refused(C::PartialRegistered {
				admin_dir: admin_dir.clone(),
			}));
		}
		// A checkout present but not registered (or an inconsistent one) is a partial-conflicting state.
		if inspection.destination_kind == DestinationKind::LinkedWorktreeCheckout {
			return Err(CreateError::Refused(C::PartialConflicting {
				detail: inspection.cross_pointers.clone(),
			}));
		}

		// A free destination: decide from the target's branch intent.
		match target {
			CheckoutTarget::NewBranch { name, .. } => match &inspection.requested_branch {
				// Strict `git -b`: the branch must not already exist. Adopting an existing ref (even at the
				// requested start) is unsafe — it cannot be told apart from a branch a user created
				// independently. An interrupted `-b` is completed via `ExistingBranch`; a fully-present
				// matching worktree was already handled as idempotent above.
				RequestedBranch::Exists { .. } => Err(CreateError::BranchExists(name.short().to_owned())),
				RequestedBranch::Absent { .. } => Ok(Action::Write {
					create_branch: true,
				}),
				RequestedBranch::NotRequested => unreachable!("a branch target sets expected_branch"),
			},
			CheckoutTarget::ExistingBranch { name, .. } => match &inspection.requested_branch {
				// The branch exists; check it out. A live worktree on this branch was already reconciled
				// above (via `matches_target`, which honours `expected_start` against the worktree HEAD); with
				// no worktree yet there is no HEAD to relate `expected_start` to, so its ancestry is validated
				// in `write_worktree` against the branch tip before anything is published.
				RequestedBranch::Exists { .. } => Ok(Action::Write {
					create_branch: false,
				}),
				_ => Err(CreateError::BranchNotFound(name.short().to_owned())),
			},
			CheckoutTarget::Detached { .. } => Ok(Action::Write {
				create_branch: false,
			}),
			CheckoutTarget::Orphan { name } => match &inspection.requested_branch {
				RequestedBranch::Exists { .. } => Err(CreateError::BranchExists(name.short().to_owned())),
				_ => Ok(Action::Write {
					create_branch: false,
				}),
			},
		}
	}

	/// Whether the start relation permits treating the branch as the requested one — the branch is *at* the
	/// requested start (`Equal`) or has *advanced* past it (`Ancestor`); a rewound/diverged branch is not.
	fn start_relation_ok(relation: Option<crate::StartRelation>) -> bool {
		matches!(
			relation,
			Some(crate::StartRelation::Equal) | Some(crate::StartRelation::Ancestor)
		)
	}

	/// Whether a *present* worktree already **is** the requested one. (Structural conflicts and wrong-branch
	/// cases were already refused by [`decide`], so a symbolic head here is on the requested branch.) A
	/// branch target additionally requires the branch to be at, or advanced past, the requested start
	/// (never diverged); a detached target requires exactly the requested commit; an orphan an unborn HEAD.
	fn matches_target(inspection: &WorktreeInspection, target: &CheckoutTarget) -> bool {
		let Some(head) = &inspection.head else {
			return false;
		};
		match target {
			CheckoutTarget::NewBranch { .. } => {
				head.state == HeadKind::Symbolic && start_relation_ok(inspection.start_relation)
			}
			CheckoutTarget::ExistingBranch { expected_start, .. } => {
				head.state == HeadKind::Symbolic
					&& (expected_start.is_none() || start_relation_ok(inspection.start_relation))
			}
			CheckoutTarget::Detached { start } => {
				head.state == HeadKind::Detached && head.object.as_ref() == Some(start)
			}
			CheckoutTarget::Orphan { .. } => head.state == HeadKind::Unborn,
		}
	}

	/// The read-only query that mirrors this create request's identity + intent, so inspection surfaces
	/// exactly the state the create decides on. `ExistingBranch` carries no start (it checks out at the
	/// branch tip), so no ancestry relation is requested for it.
	fn request_query(request: &CreateRequest) -> WorktreeQuery {
		let (expected_branch, start) = match &request.target {
			CheckoutTarget::NewBranch { name, start } => (Some(name.clone()), Some(start.clone())),
			CheckoutTarget::ExistingBranch {
				name,
				expected_start,
			} => (Some(name.clone()), expected_start.clone()),
			CheckoutTarget::Detached { start } => (None, Some(start.clone())),
			CheckoutTarget::Orphan { name } => (Some(name.clone()), None),
		};
		WorktreeQuery {
			repo: request.repo.clone(),
			destination: request.destination.clone(),
			expected_branch,
			start,
		}
	}

	/// The other-worktree checkout of the requested branch, if any (a branch-use conflict).
	fn requested_checked_out_elsewhere(requested: &RequestedBranch) -> Option<&Path> {
		match requested {
			RequestedBranch::Exists {
				checked_out_elsewhere,
				..
			}
			| RequestedBranch::Absent {
				checked_out_elsewhere,
			} => checked_out_elsewhere.as_deref(),
			RequestedBranch::NotRequested => None,
		}
	}

	/// Validate a branch/orphan target's name with git's `check-ref-format --branch` rules: a valid
	/// `refs/heads/<name>` refname that additionally does not start with `-` and is not `HEAD`.
	fn validate_target_name(target: &CheckoutTarget) -> Result<(), CreateError> {
		let name = match target {
			CheckoutTarget::NewBranch { name, .. }
			| CheckoutTarget::ExistingBranch { name, .. }
			| CheckoutTarget::Orphan { name } => name,
			CheckoutTarget::Detached { .. } => return Ok(()),
		};
		let short = name.short();
		let valid = !short.starts_with('-') && short != "HEAD" && is_valid_refname(&name.refname());
		if valid {
			Ok(())
		} else {
			Err(CreateError::InvalidBranchName(short.to_owned()))
		}
	}

	/// Write the branch (when creating), the admin layout, and the checkout for `request`.
	async fn write_worktree<H: HashAlgorithm>(
		request: &CreateRequest,
		effective: Option<&GitConfig>,
		create_branch: bool,
	) -> Result<(), CreateError>
	where
		ObjectId<H>: IntoWorktreeObjectId,
	{
		let common = request.repo.common_dir();
		let destination = &request.destination;

		// The shared repository: source of the branch ref, the committer identity, and the reflog policy.
		let mut repo = Repository::<_, H>::new(ObjectStore::new(open_store_raw(common, common)?));
		if let Some(config) = effective {
			repo.set_effective_config(config.clone());
		}
		let config = repo
			.effective_config()
			.await
			.map_err(LinkedWorktreeError::Repository)?;
		let committer = committer_line(&config);

		// The `HEAD` to write, the start commit (the checkout source + `ORIG_HEAD`, `None` for an orphan),
		// and the branch ref name when this is a branch worktree.
		let (head, start, refname): (HeadState<H>, Option<ObjectId<H>>, Option<String>) =
			match &request.target {
				CheckoutTarget::NewBranch { name, start } => {
					let start = to_object_id::<H>(start)?;
					(
						HeadState::Symbolic(name.refname()),
						Some(start),
						Some(name.refname()),
					)
				}
				CheckoutTarget::ExistingBranch {
					name,
					expected_start,
				} => {
					// Check out the existing branch at its current tip.
					let tip = repo
						.refs()
						.resolve_symbolic(&name.refname())
						.await
						.map_err(LinkedWorktreeError::Repository)?
						.ok_or_else(|| CreateError::BranchNotFound(name.short().to_owned()))?;
					// Reconciling with an expected start: the branch must be *at* it or *descended from* it —
					// completing an interrupted create, never checking out history that has since diverged.
					// (A live worktree was reconciled earlier via `matches_target`; this covers the no-worktree
					// case, where there is no HEAD for `start_relation` to have measured.)
					if let Some(expected) = expected_start {
						let expected = to_object_id::<H>(expected)?;
						let compatible = expected == tip
							|| repo
								.is_ancestor(expected, tip)
								.await
								.map_err(LinkedWorktreeError::Repository)?;
						if !compatible {
							return Err(CreateError::Refused(
								WorktreeClassification::IdentityConflict {
									detail: crate::IdentityConflict::BranchAtUnexpectedObject { found: tip.tag() },
								},
							));
						}
					}
					(HeadState::Symbolic(name.refname()), Some(tip), None)
				}
				CheckoutTarget::Detached { start } => {
					let start = to_object_id::<H>(start)?;
					(HeadState::Detached(start), Some(start), None)
				}
				CheckoutTarget::Orphan { name } => (HeadState::Symbolic(name.refname()), None, None),
			};

		// Validate the start commit *before* mutating anything: a missing / non-commit object must not
		// publish a branch or admin layout that a retry would then misread as complete.
		let tree = match start {
			Some(start) => Some(
				repo
					.commit_tree(start)
					.await
					.map_err(LinkedWorktreeError::Repository)?,
			),
			None => None,
		};

		// Create the branch through the transactional ref layer (CAS: it must not already exist).
		if create_branch {
			let refname = refname.expect("a NewBranch worktree implies a branch ref name");
			let start = start.expect("a NewBranch worktree implies a start commit");
			let message = format!("branch: Created from {}", start.to_hex());
			repo
				.refs()
				.transact(&[RefOp {
					name: refname,
					expected: None,
					new: Some(start),
					reflog: ReflogIntent::Log {
						committer: &committer,
						message: &message,
					},
				}])
				.await
				.map_err(|(_, error)| LinkedWorktreeError::Repository(error))?;
		}

		// Seed the new worktree's per-worktree `logs/HEAD` under the **non-bare** default — the linked
		// worktree is non-bare even when the host repository is bare, so git logs its HEAD unless
		// `core.logAllRefUpdates` is *explicitly* disabled (not the host's bare default of off).
		let log_head = head_reflog_enabled(&config);
		let admin = unique_admin_dir(common, destination)?;
		let admin = write_admin_layout(&admin, destination, &head, start, &committer, log_head)?;

		// Materialise the checkout (index + files) in the new worktree's namespace, *then* write the
		// checkout's `.git` gitfile last — the commit point that makes the registration live. An orphan has
		// no start, so it gets a valid zero-entry index and an empty checkout (as git does).
		let new_repo = Repository::<_, H>::new(ObjectStore::new(open_store_raw(&admin, common)?));
		let work = open_work_dir(destination)?;
		let worktree = WorkTree::new(new_repo, work, admin.clone());
		match tree {
			Some(tree) => worktree
				.checkout(tree, false)
				.await
				.map_err(LinkedWorktreeError::Worktree)?,
			// Orphan: no tree to lay down, but git still writes a zero-entry index immediately.
			None => worktree
				.save_index(&Index::new())
				.await
				.map_err(LinkedWorktreeError::Worktree)?,
		}
		write_checkout_gitfile(destination, &admin)?;
		Ok(())
	}

	/// The committer line (`Name <email> seconds ±hhmm`) for the new worktree's reflogs, resolved from the
	/// repository's effective config — defaulting a missing identity to git's reflog placeholder rather
	/// than failing, as git records reflog lines without a configured identity.
	fn committer_line(config: &GitConfig) -> String {
		let secs = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(0);
		let when = format!("{secs} +0000");
		gitana_identity::signature_or_default(None, None, Some(config), &when)
	}

	/// Whether the new (non-bare) linked worktree seeds its per-worktree `logs/HEAD`: enabled unless
	/// `core.logAllRefUpdates` is *explicitly* `false`. git's non-bare default is on, so a worktree created
	/// even from a bare host still gets its HEAD reflog (while the host's shared branch reflogs stay off).
	fn head_reflog_enabled(config: &GitConfig) -> bool {
		!matches!(
			config.get_bool("core", None, "logallrefupdates"),
			Ok(Some(false))
		)
	}

	/// Convert a runtime-tagged object id to this repository's `ObjectId<H>` — an id of a different hash
	/// format cannot belong to the repository (inspection already rejected it, so this is a guard).
	fn to_object_id<H: HashAlgorithm>(
		id: &WorktreeObjectId,
	) -> Result<ObjectId<H>, LinkedWorktreeError> {
		ObjectId::<H>::from_hex(&id.to_hex()).map_err(|_| LinkedWorktreeError::InvalidObjectId {
			kind: id.kind(),
			hex: id.to_hex(),
		})
	}

	/// The admin directory for a new worktree named after the destination's basename, uniquified against
	/// the existing `<common>/worktrees/*` (git appends `1`, `2`, … on collision).
	fn unique_admin_dir(common: &Path, destination: &Path) -> Result<PathBuf, LinkedWorktreeError> {
		let base = destination
			.file_name()
			.and_then(|name| name.to_str())
			.ok_or_else(|| {
				LinkedWorktreeError::io(
					"deriving worktree name",
					destination,
					std::io::Error::from(std::io::ErrorKind::InvalidInput),
				)
			})?;
		// A candidate is free only when *nothing* — not even a dangling symlink — sits at the path, so
		// probe with non-following `symlink_metadata`: `Path::exists()` reports a broken symlink as absent
		// and we would then pick an occupied name and fail `create_dir_all` *after* publishing the branch.
		let occupied = |path: &Path| path.symlink_metadata().is_ok();
		let worktrees = common.join("worktrees");
		if !occupied(&worktrees.join(base)) {
			return Ok(worktrees.join(base));
		}
		for suffix in 1u32.. {
			let candidate = worktrees.join(format!("{base}{suffix}"));
			if !occupied(&candidate) {
				return Ok(candidate);
			}
		}
		unreachable!("a free worktree admin name always exists")
	}

	/// Write git's admin layout for the new worktree — the admin's `gitdir` back-pointer, `commondir`,
	/// `HEAD`, and (for a non-orphan) `ORIG_HEAD` plus a seeded `logs/HEAD` — returning the canonical admin
	/// path. The **checkout's** `.git` gitfile is *not* written here (see [`write_checkout_gitfile`]).
	fn write_admin_layout<H: HashAlgorithm>(
		admin: &Path,
		destination: &Path,
		head: &HeadState<H>,
		start: Option<ObjectId<H>>,
		committer: &str,
		log_head: bool,
	) -> Result<PathBuf, LinkedWorktreeError> {
		std::fs::create_dir_all(admin)
			.map_err(|e| LinkedWorktreeError::io("creating admin dir", admin, e))?;
		std::fs::create_dir_all(destination)
			.map_err(|e| LinkedWorktreeError::io("creating checkout dir", destination, e))?;

		// Absolute cross-pointer, so each side resolves regardless of the process cwd.
		let admin = admin
			.canonicalize()
			.map_err(|e| LinkedWorktreeError::io("resolving admin dir", admin, e))?;
		let destination = destination
			.canonicalize()
			.map_err(|e| LinkedWorktreeError::io("resolving checkout dir", destination, e))?;
		let gitfile = destination.join(".git");

		let write = |path: PathBuf, contents: String| {
			std::fs::write(&path, contents)
				.map_err(move |e| LinkedWorktreeError::io("writing admin file", path, e))
		};
		// `commondir` is relative (git writes `../..`): from `<common>/worktrees/<name>` up to `<common>`.
		write(admin.join("commondir"), "../..\n".to_owned())?;
		write(admin.join("gitdir"), format!("{}\n", gitfile.display()))?;
		write(admin.join("HEAD"), head.render())?;
		if let Some(start) = start {
			write(admin.join("ORIG_HEAD"), format!("{start}\n"))?;
			if log_head {
				write_head_reflog(&admin, head, start, committer)?;
			}
		}
		Ok(admin)
	}

	/// Write the checkout's `.git` gitfile — **last**, after the checkout has materialised — so a partial
	/// create (interrupted before this) reads as `PartialRegistered`, never a false "complete".
	///
	/// Created **exclusively and without following symlinks** (`O_CREAT|O_EXCL`): inspection already found
	/// the destination free, so a `.git` present now was raced in after the check — refuse with a
	/// destination conflict rather than truncating an unknown file or following a symlink into its target.
	fn write_checkout_gitfile(destination: &Path, admin: &Path) -> Result<(), CreateError> {
		use std::io::Write as _;

		let destination = destination
			.canonicalize()
			.map_err(|e| LinkedWorktreeError::io("resolving checkout dir", destination, e))?;
		let gitfile = destination.join(".git");
		let mut file = match std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&gitfile)
		{
			Ok(file) => file,
			Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
				return Err(CreateError::Refused(
					WorktreeClassification::DestinationConflict {
						kind: DestinationKind::OtherFsObject,
					},
				));
			}
			Err(e) => return Err(LinkedWorktreeError::io("writing checkout .git", gitfile, e).into()),
		};
		file
			.write_all(format!("gitdir: {}\n", admin.display()).as_bytes())
			.map_err(|e| LinkedWorktreeError::io("writing checkout .git", gitfile, e))?;
		Ok(())
	}

	/// Seed the new worktree's per-worktree `logs/HEAD` exactly as `git worktree add` does: a creation line
	/// (`0…0 <commit> <committer>`, no message), then — only when `HEAD` is a branch — a
	/// `<commit> <commit> <committer>\treset: moving to HEAD` line.
	fn write_head_reflog<H: HashAlgorithm>(
		admin: &Path,
		head: &HeadState<H>,
		start: ObjectId<H>,
		committer: &str,
	) -> Result<(), LinkedWorktreeError> {
		let zero = "0".repeat(H::RAW_LEN * 2);
		let oid = start.to_hex();
		let mut log = format!("{zero} {oid} {committer}\n");
		if matches!(head, HeadState::Symbolic(_)) {
			log.push_str(&format!("{oid} {oid} {committer}\treset: moving to HEAD\n"));
		}
		let logs = admin.join("logs");
		std::fs::create_dir_all(&logs)
			.map_err(|e| LinkedWorktreeError::io("creating logs dir", &logs, e))?;
		let head_log = logs.join("HEAD");
		std::fs::write(&head_log, log)
			.map_err(|e| LinkedWorktreeError::io("writing logs/HEAD", head_log, e))
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::create;
