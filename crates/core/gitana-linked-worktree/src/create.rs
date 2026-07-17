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
	use std::ffi::OsStr;
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
	use crate::registration_lock::RegistrationLock;
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

		// Serialize registration mutations for the repository so a lost race is a **conflict, not an
		// overwrite**: hold the per-repository lock across the whole inspect→decide→write→re-decide section,
		// so a concurrent create/remove cannot pick the same admin name and clobber, nor slip a registration
		// in between the write and the post-condition re-decide. Released on any return (and on cancellation).
		let _lock = RegistrationLock::acquire(request.repo.common_dir()).await?;

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

		// Re-inspect *and re-decide*: success only when the now-established state actually **is** the
		// requested worktree. A legitimate create always re-decides to `AlreadyThere` (the worktree it just
		// wrote matches the request); **any** other post-write outcome — `decide` wanting another write, or
		// `decide` refusing (a concurrent branch divergence, a re-appeared conflict, a removed destination) —
		// means a race changed the state out from under us, so it is one `NotEstablished` lost-race error
		// carrying the observed post-state, never a preflight-style refusal or a false success. (A genuine
		// I/O failure during the re-inspect still propagates as `Failed` via `?`.)
		let established = inspect(&query).await?;
		match decide(&established, &request.target) {
			Ok(Action::AlreadyThere) => Ok(established),
			Ok(Action::Write { .. }) | Err(_) => Err(CreateError::NotEstablished(Box::new(established))),
		}
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
		// A **recoverable mid-checkout partial** (an owned registration whose checkout is gone **and the
		// destination is absent or empty**, with no more-specific refusal): refuse as `PartialRegistered` — a
		// prune-and-retry state — rather than letting an empty/absent partial read as a `DestinationConflict`.
		// Mirrors `classify`'s recoverable read (same precedence: empty/absent destination, no identity
		// conflict, and the requested branch not force-checked-out elsewhere — a branch-use conflict out-ranks
		// it). A *non-empty* directory at the path is not verifiably this create's own content, so it falls
		// through to the destination-content refusal below. Write-path side of the "recoverable mid-checkout".
		if inspection.identity_conflict.is_none()
			&& matches!(
				inspection.destination_kind,
				DestinationKind::Absent | DestinationKind::EmptyDir
			) && let Registration::PresentCheckoutMissing { admin_dir } = &inspection.registration
		{
			if let Some(other) = requested_checked_out_elsewhere(&inspection.requested_branch) {
				return Err(CreateError::Refused(C::BranchUseConflict {
					other_checkout: other.to_path_buf(),
				}));
			}
			return Err(CreateError::Refused(C::PartialRegistered {
				admin_dir: admin_dir.clone(),
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
			// Create decides from registration/destination/branch facts; a worktree's cleanliness never gates a
			// create (it either reconciles an exact match or refuses a conflict), so skip the status scan.
			with_status: false,
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

		// Choose the admin directory. The pointer files (admin `gitdir` → the checkout `.git`, checkout
		// `.git` → the admin) are serialized **byte-clean** (see `pointers::path_to_bytes`), so a non-UTF-8
		// identity path round-trips exactly — native paths are accepted without UTF-8 conversion, as required.
		let admin = unique_admin_dir(common, destination)?;
		// On a platform without byte-clean pointer I/O (non-Unix), a non-representable path can't round-trip
		// the pointers, so reject it **up front** — before the branch/admin/checkout are written — rather than
		// mutate then fail the post-write inspection. A no-op on Unix (the pointers are byte-clean there).
		ensure_representable_path(destination)?;
		ensure_representable_path(&admin)?;

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

	/// The admin directory for a new worktree, named after the destination's basename — *sanitized* the way
	/// git sanitizes it (see [`sanitize_worktree_name`]) and uniquified against the existing
	/// `<common>/worktrees/*` (git appends `1`, `2`, … to the sanitized name on collision).
	fn unique_admin_dir(common: &Path, destination: &Path) -> Result<PathBuf, LinkedWorktreeError> {
		// Sanitize the basename's bytes into a refname-safe admin name (git keeps bytes ≥ 0x80 — so a
		// non-UTF-8 basename keeps its high bytes — and maps only refname-invalid ASCII); the admin `gitdir`
		// still records the *real* (unsanitized) destination path, byte-clean.
		let base = sanitize_worktree_name(&crate::pointers::path_to_bytes(Path::new(admin_base_name(
			destination,
		)?)));
		let worktrees = common.join("worktrees");
		// A candidate is free only when *nothing* — not even a dangling symlink — sits at the path, so
		// probe with non-following `symlink_metadata`: `Path::exists()` reports a broken symlink as absent
		// and we would then pick an occupied name and fail `create_dir_all` *after* publishing the branch.
		let occupied = |path: &Path| path.symlink_metadata().is_ok();
		// git appends the numeric suffix to the *sanitized* name (digits are always refname-safe). Build the
		// name in bytes so a non-UTF-8 sanitized name is preserved.
		let candidate = |suffix: Option<u32>| -> PathBuf {
			let mut name = base.clone();
			if let Some(n) = suffix {
				name.extend_from_slice(n.to_string().as_bytes());
			}
			worktrees.join(crate::pointers::os_string_from_bytes(&name))
		};
		let first = candidate(None);
		if !occupied(&first) {
			return Ok(first);
		}
		for suffix in 1u32.. {
			let c = candidate(Some(suffix));
			if !occupied(&c) {
				return Ok(c);
			}
		}
		unreachable!("a free worktree admin name always exists")
	}

	/// The destination's basename as a native `OsStr` — **no UTF-8 requirement** (it becomes the sanitized
	/// admin directory name, and the pointer files record the real path byte-clean). An absent basename (a
	/// path ending in `/` or `..`) is an error.
	fn admin_base_name(destination: &Path) -> Result<&OsStr, LinkedWorktreeError> {
		destination.file_name().ok_or_else(|| {
			LinkedWorktreeError::io(
				"deriving worktree name",
				destination,
				std::io::Error::from(std::io::ErrorKind::InvalidInput),
			)
		})
	}

	/// Ensure a path can round-trip the (byte-clean) pointer I/O before any state is written. On **Unix**
	/// the pointers are byte-clean, so this is a no-op — a non-UTF-8 path is accepted. On **non-Unix**, where
	/// `path_to_bytes` still falls back to a *lossy* UTF-8 rendering, a non-UTF-8 path would serialize to a
	/// back-pointer that no longer identifies the destination; reject it here (before the branch/admin/
	/// checkout are written) so `create` never mutates state it would then fail to establish. Windows WTF-8
	/// pointer I/O is a deferred follow-up.
	#[cfg(unix)]
	fn ensure_representable_path(_path: &Path) -> Result<(), LinkedWorktreeError> {
		Ok(())
	}

	#[cfg(not(unix))]
	fn ensure_representable_path(path: &Path) -> Result<(), LinkedWorktreeError> {
		// Check the **resolved** form the pointer files will actually record — a symlink/junction can resolve
		// a UTF-8 lexical path to a non-representable one, which would then serialize lossily *after* the
		// branch/admin/checkout are written. Rejecting it here keeps `create` side-effect-free on failure.
		if resolved_for_pointers(path).to_str().is_some() {
			Ok(())
		} else {
			Err(LinkedWorktreeError::io(
				"non-UTF-8 path is unsupported on this platform (byte-clean pointer I/O is Unix-only)",
				path,
				std::io::Error::from(std::io::ErrorKind::InvalidInput),
			))
		}
	}

	/// The form of `path` the pointer files will actually record — its deepest existing ancestor
	/// canonicalized (so a symlinked parent is resolved to its real target, exactly as `create_dir_all` +
	/// `canonicalize` would), with the still-absent tail appended lexically. Used only by the non-Unix
	/// representability preflight; on Unix the pointers are byte-clean so no such check is needed.
	#[cfg(not(unix))]
	fn resolved_for_pointers(path: &Path) -> PathBuf {
		use std::path::Component;
		let mut resolved = PathBuf::new();
		for component in path.components() {
			match component {
				Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
				Component::RootDir => resolved.push(Component::RootDir.as_os_str()),
				Component::CurDir => {}
				Component::ParentDir => {
					resolved.pop();
				}
				Component::Normal(name) => {
					resolved.push(name);
					if let Ok(canonical) = resolved.canonicalize() {
						resolved = canonical;
					}
				}
			}
		}
		resolved
	}

	/// Sanitize a destination basename into a worktree admin-directory name, mirroring git (probed against
	/// git 2.50.1). git reuses the admin name as a per-worktree ref namespace, so it must be a valid refname
	/// *component*: a refname-invalid byte (a control byte, space, DEL, or one of `* : ? [ \ ^ ~`) becomes
	/// `-`, a leading `.` is neutralised, a `..` run collapses to a single `.`, an `@{` sequence is broken, a
	/// bare `@` becomes `-`, and a trailing `.lock` is stripped (repeatedly). Bytes ≥ `0x80` pass through
	/// unchanged. This keeps the admin *path* free of the newline/CR that would otherwise break the gitfile
	/// cross-pointers — while the admin `gitdir` still records the real (unsanitized) destination path.
	fn sanitize_worktree_name(name: &[u8]) -> Vec<u8> {
		let mut out = Vec::with_capacity(name.len());
		for (i, &b) in name.iter().enumerate() {
			let refname_bad = b < 0x20
				|| b == 0x7f
				|| matches!(b, b' ' | b'*' | b':' | b'?' | b'[' | b'\\' | b'^' | b'~');
			if refname_bad {
				out.push(b'-');
			} else if b == b'.' {
				if i == 0 {
					out.push(b'-'); // a component may not start with '.'
				} else if name[i - 1] != b'.' {
					out.push(b'.'); // keep a single dot; a '..' run collapses (skip the repeat)
				}
			} else if b == b'{' && i > 0 && name[i - 1] == b'@' {
				out.push(b'-'); // break the forbidden '@{' sequence
			} else {
				out.push(b);
			}
		}
		// A refname component may not be a bare `@` (git's HEAD shorthand) → `-`. Applied *before* the `.lock`
		// strip so `@.lock` still becomes `@` (the strip leaves a bare `@`, which git accepts — probed), while
		// a literal `@` basename becomes `-`.
		if out == b"@" {
			out = vec![b'-'];
		}
		// A refname component may not end in '.lock'; git strips it (and re-checks, so '.lock.lock' → '').
		while out.ends_with(b".lock") {
			out.truncate(out.len() - 5);
		}
		// A basename never sanitizes to empty in practice (a leading '.' becomes '-'); guard defensively so
		// a pathological all-stripped name still yields a usable directory rather than the worktrees root.
		if out.is_empty() {
			out.push(b'-');
		}
		out
	}

	/// A unique temp sibling of `path` (`<name>.tmp.<pid>.<seq>`) for the write-then-rename dance. Unique per
	/// process + call so two creates targeting the same directory never collide on the staging file.
	fn temp_sibling(path: &Path) -> PathBuf {
		use std::sync::atomic::{AtomicU64, Ordering};
		static SEQ: AtomicU64 = AtomicU64::new(0);
		let seq = SEQ.fetch_add(1, Ordering::Relaxed);
		let mut name = path
			.file_name()
			.map(|n| n.to_os_string())
			.unwrap_or_default();
		name.push(format!(".tmp.{}.{}", std::process::id(), seq));
		path.with_file_name(name)
	}

	/// Create `path` exclusively, write `contents`, and `fsync` — so the file's bytes are durable before it
	/// is published. The caller then links/renames it into its final name.
	fn write_and_sync(path: &Path, contents: &[u8]) -> Result<(), LinkedWorktreeError> {
		use std::io::Write as _;
		let mut file = std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(path)
			.map_err(|e| LinkedWorktreeError::io("creating temp file", path, e))?;
		file
			.write_all(contents)
			.map_err(|e| LinkedWorktreeError::io("writing temp file", path, e))?;
		file
			.sync_all()
			.map_err(|e| LinkedWorktreeError::io("syncing temp file", path, e))
	}

	/// Publish `contents` at `path` atomically: fully write a temp sibling, then `rename` it onto `path`
	/// (replacing) — a reader never observes a torn pointer, and a crash leaves the target absent (a
	/// classifiable partial state) rather than a half-written file (a malformed-pointer hard error).
	fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<(), LinkedWorktreeError> {
		let tmp = temp_sibling(path);
		write_and_sync(&tmp, contents)?;
		std::fs::rename(&tmp, path).map_err(|e| {
			let _ = std::fs::remove_file(&tmp);
			LinkedWorktreeError::io("publishing admin file", path, e)
		})
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

		let write = |path: PathBuf, contents: String| write_file_atomic(&path, contents.as_bytes());
		// `commondir` is relative (git writes `../..`): from `<common>/worktrees/<name>` up to `<common>`.
		write(admin.join("commondir"), "../..\n".to_owned())?;
		// `gitdir` records the checkout's real `.git` path, serialized **byte-clean** so a non-UTF-8
		// destination round-trips exactly (`Path::display()` would lose a non-UTF-8 byte).
		let mut gitdir_bytes = crate::pointers::path_to_bytes(&gitfile);
		gitdir_bytes.push(b'\n');
		write_file_atomic(&admin.join("gitdir"), &gitdir_bytes)?;
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
	/// Created **exclusively and without following symlinks** (`O_CREAT | O_EXCL`, exactly as git writes it):
	/// inspection already found the destination free, so a `.git` present now was raced in after the check —
	/// refuse it as a destination conflict rather than truncating an unknown file or following a symlink into
	/// its target. This is written *directly* (not via a temp + rename/link): the destination lives on an
	/// arbitrary filesystem — including ones without hard-link support (FAT/exFAT, some SMB/NFS) where a
	/// link-based publish would fail even though git succeeds — and an `O_EXCL` create is the portable
	/// no-clobber primitive. It leaves no working-tree staging file to become untracked clutter.
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
		// Byte-clean serialization of the admin path, so a non-UTF-8 admin path round-trips exactly.
		let mut content = b"gitdir: ".to_vec();
		content.extend_from_slice(&crate::pointers::path_to_bytes(admin));
		content.push(b'\n');
		file
			.write_all(&content)
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
		write_file_atomic(&head_log, log.as_bytes())
	}

	#[cfg(test)]
	mod sanitize_tests {
		use super::sanitize_worktree_name;

		fn san(s: &[u8]) -> String {
			String::from_utf8(sanitize_worktree_name(s)).unwrap()
		}

		#[test]
		fn replaces_refname_invalid_bytes_with_dash() {
			// Control bytes, space, DEL, and `* : ? [ \ ^ ~` are refname-invalid → `-` (probed vs git 2.50.1).
			assert_eq!(san(b"wt\nx"), "wt-x");
			assert_eq!(san(b"wt\rx"), "wt-x");
			assert_eq!(san(b"wt\tx"), "wt-x");
			assert_eq!(san(b"a b"), "a-b");
			assert_eq!(san(b"a\x7fb"), "a-b");
			assert_eq!(san(b"a*b"), "a-b");
			assert_eq!(san(b"a:b"), "a-b");
			assert_eq!(san(b"a?b"), "a-b");
			assert_eq!(san(b"a[b"), "a-b");
			assert_eq!(san(b"a\\b"), "a-b");
			assert_eq!(san(b"a^b"), "a-b");
			assert_eq!(san(b"a~b"), "a-b");
		}

		#[test]
		fn keeps_refname_legal_punctuation_and_high_bytes() {
			// `! " # $ % & ' ( ) + , . ; < = > @ ] _ \` { | }` and letters/digits/high bytes pass through.
			assert_eq!(san(b"ok_name-1.2"), "ok_name-1.2");
			assert_eq!(san(b"a@b"), "a@b");
			assert_eq!(san(b"a{b"), "a{b"); // a lone `{` is fine; only `@{` is forbidden
			assert_eq!(san("wPéQ".as_bytes()), "wPéQ");
		}

		#[test]
		fn applies_refname_component_rules() {
			assert_eq!(san(b".foo"), "-foo"); // a component may not start with '.'
			assert_eq!(san(b"a..b"), "a.b"); // no '..'
			assert_eq!(san(b"a...b"), "a.b"); // a run collapses
			assert_eq!(san(b"foo."), "foo."); // a *trailing* dot is left as-is (git does)
			assert_eq!(san(b"x@{y"), "x@-y"); // break the '@{' sequence
			assert_eq!(san(b"@"), "-"); // a bare '@' (HEAD shorthand) -> '-'
			assert_eq!(san(b"@@"), "@@"); // ...but only a *bare* '@'
			assert_eq!(san(b"a@b"), "a@b"); // '@' mid-name is fine
			assert_eq!(san(b"@.lock"), "@"); // '.lock' strip may leave a bare '@' — git accepts that
			assert_eq!(san(b"x.lock"), "x"); // no trailing '.lock'
			assert_eq!(san(b"x.lock.lock"), "x"); // stripped repeatedly
			assert_eq!(san(b"x.LOCK"), "x.LOCK"); // case-sensitive
			assert_eq!(san(b"x.locked"), "x.locked"); // only a whole trailing '.lock'
			assert_eq!(san(b"x.git"), "x.git"); // '.git' is fine
		}
	}
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::create;
