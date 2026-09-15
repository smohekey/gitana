use crate::{ObjectReadLimits, ObjectStoreError};

pub(crate) struct ObjectReadBudget<'a> {
	pub(crate) limits: &'a ObjectReadLimits,
	backing_bytes: u64,
	expanded_bytes: u64,
	index_bytes: u64,
}

impl<'a> ObjectReadBudget<'a> {
	pub(crate) fn new(limits: &'a ObjectReadLimits) -> Self {
		Self {
			limits,
			backing_bytes: 0,
			expanded_bytes: 0,
			index_bytes: 0,
		}
	}

	pub(crate) fn charge_backing(&mut self, bytes: u64) -> Result<(), ObjectStoreError> {
		charge(
			&mut self.backing_bytes,
			bytes,
			self.limits.max_backing_bytes(),
			"backing-bytes",
		)
	}

	pub(crate) fn charge_expanded(&mut self, bytes: u64) -> Result<(), ObjectStoreError> {
		charge(
			&mut self.expanded_bytes,
			bytes,
			self.limits.max_expanded_bytes(),
			"expanded-bytes",
		)
	}

	pub(crate) fn charge_index(&mut self, bytes: u64) -> Result<(), ObjectStoreError> {
		charge(
			&mut self.index_bytes,
			bytes,
			self.limits.max_index_bytes(),
			"index-bytes",
		)
	}

	pub(crate) fn remaining_expanded(&self) -> u64 {
		self
			.limits
			.max_expanded_bytes()
			.saturating_sub(self.expanded_bytes)
	}
}

fn charge(
	used: &mut u64,
	bytes: u64,
	limit: u64,
	resource: &'static str,
) -> Result<(), ObjectStoreError> {
	let next = used
		.checked_add(bytes)
		.ok_or(ObjectStoreError::ReadLimitExceeded { resource, limit })?;
	if next > limit {
		return Err(ObjectStoreError::ReadLimitExceeded { resource, limit });
	}
	*used = next;
	Ok(())
}
