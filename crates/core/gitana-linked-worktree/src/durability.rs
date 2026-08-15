//! Targeted durability barriers for linked-worktree creation and removal.

use std::path::Path;

use gitana_file_store::{DurabilityTarget, FileStore};
use gitana_file_store_local::LocalFileStore;

use crate::admin_cleanup::{open_directory_nofollow, path_absent, remove_empty_directory};
use crate::create::{matches_target, request_query};
use crate::{
	CreateError, CreateRequest, CrossPointerHealth, DestinationKind, LinkedWorktreeError,
	Registration, RemoveError, RemoveRequest, WorktreeInspection, WorktreeQuery, inspect,
};

/// Flush a complete created worktree's checkout and administration before a caller records its
/// semantic completion.
///
/// This barrier validates the exact request both before and after flushing. It synchronizes regular
/// files in the checkout and admin trees first, then the `worktrees` and checkout-parent namespaces
/// that publish those directories. The shared branch itself is intentionally separate: callers bind
/// and flush it with [`gitana_repository::Repository::durability_barrier_ref`].
pub async fn durability_barrier_created(
	request: &CreateRequest,
) -> Result<WorktreeInspection, CreateError> {
	let query = request_query(request);
	let before = inspect(&query).await?;
	require_created(request, &before)?;
	let admin = before
		.git_dir
		.as_deref()
		.ok_or_else(|| CreateError::NotEstablished(Box::new(before.clone())))?;

	sync_tree(&request.destination, "syncing worktree checkout").await?;
	sync_tree(admin, "syncing worktree admin").await?;
	sync_directory(
		admin
			.parent()
			.ok_or_else(|| CreateError::NotEstablished(Box::new(before.clone())))?,
		"syncing worktree namespace",
	)
	.await?;
	sync_directory(
		request
			.destination
			.parent()
			.ok_or_else(|| CreateError::NotEstablished(Box::new(before.clone())))?,
		"syncing checkout parent namespace",
	)
	.await?;
	sync_directory(request.repo.common_dir(), "syncing repository namespace").await?;

	let after = inspect(&query).await?;
	require_created(request, &after)?;
	if before == after {
		Ok(after)
	} else {
		Err(CreateError::NotEstablished(Box::new(after)))
	}
}

/// Flush the checkout and administration namespaces after a linked worktree has been removed.
///
/// An otherwise-idempotent empty destination is removed as part of this explicit cleanup boundary.
/// Success requires both the registration and checkout to remain absent after the directory syncs.
pub async fn durability_barrier_removed(request: &RemoveRequest) -> Result<(), RemoveError> {
	let query = WorktreeQuery {
		repo: request.repo.clone(),
		destination: request.destination.clone(),
		expected_branch: request.expected_branch.clone(),
		start: None,
		with_status: false,
	};
	let before = inspect(&query).await?;
	if before.registration != Registration::None {
		return Err(RemoveError::Incomplete(Box::new(before)));
	}
	match before.destination_kind {
		DestinationKind::Absent => {}
		DestinationKind::EmptyDir => {
			remove_empty_directory(&request.destination).map_err(|error| {
				LinkedWorktreeError::io("removing empty checkout", &request.destination, error)
			})?;
		}
		_ => return Err(RemoveError::Incomplete(Box::new(before))),
	}
	if !path_absent(&request.destination) {
		return Err(RemoveError::Incomplete(Box::new(inspect(&query).await?)));
	}

	let worktrees = request.repo.common_dir().join("worktrees");
	match std::fs::symlink_metadata(&worktrees) {
		Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
			sync_directory(&worktrees, "syncing worktree namespace").await?;
		}
		Ok(_) => {
			return Err(
				LinkedWorktreeError::io(
					"inspecting worktree namespace",
					&worktrees,
					std::io::Error::new(
						std::io::ErrorKind::InvalidData,
						"worktree namespace is not a no-follow directory",
					),
				)
				.into(),
			);
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
		Err(error) => {
			return Err(
				LinkedWorktreeError::io("inspecting worktree namespace", &worktrees, error).into(),
			);
		}
	}
	sync_directory(
		request
			.destination
			.parent()
			.ok_or_else(|| RemoveError::Incomplete(Box::new(before.clone())))?,
		"syncing checkout parent namespace",
	)
	.await?;
	sync_directory(request.repo.common_dir(), "syncing repository namespace").await?;

	let after = inspect(&query).await?;
	if after.registration == Registration::None && after.destination_kind == DestinationKind::Absent {
		Ok(())
	} else {
		Err(RemoveError::Incomplete(Box::new(after)))
	}
}

fn require_created(
	request: &CreateRequest,
	inspection: &WorktreeInspection,
) -> Result<(), CreateError> {
	let complete = inspection.destination_kind == DestinationKind::LinkedWorktreeCheckout
		&& matches!(inspection.registration, Registration::Present { .. })
		&& inspection.cross_pointers == CrossPointerHealth::Consistent
		&& inspection.identity_conflict.is_none()
		&& inspection.head.is_some()
		&& matches_target(inspection, &request.target);
	if complete {
		Ok(())
	} else {
		Err(CreateError::NotEstablished(Box::new(inspection.clone())))
	}
}

async fn sync_tree(path: &Path, context: &'static str) -> Result<(), LinkedWorktreeError> {
	let directory =
		open_directory_nofollow(path).map_err(|error| LinkedWorktreeError::io(context, path, error))?;
	LocalFileStore::from_dir(directory)
		.durability_barrier(&[DurabilityTarget::tree("")])
		.await
		.map_err(|error| {
			LinkedWorktreeError::io(context, path, std::io::Error::other(error.to_string()))
		})
}

async fn sync_directory(path: &Path, context: &'static str) -> Result<(), LinkedWorktreeError> {
	let directory =
		open_directory_nofollow(path).map_err(|error| LinkedWorktreeError::io(context, path, error))?;
	LocalFileStore::from_dir(directory)
		.durability_barrier(&[DurabilityTarget::directory("")])
		.await
		.map_err(|error| {
			LinkedWorktreeError::io(context, path, std::io::Error::other(error.to_string()))
		})
}
