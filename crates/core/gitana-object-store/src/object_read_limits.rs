/// Resource limits for one untrusted object read.
///
/// These limits cover both loose and packed storage. `max_backing_bytes` counts compressed loose
/// bytes or pack-entry ranges read for the object and its delta bases. `max_expanded_bytes` counts
/// every inflated base/delta and every intermediate delta result, so a deep chain cannot repeatedly
/// materialise `max_object_bytes` objects without exhausting an aggregate budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectReadLimits {
	max_object_bytes: u64,
	max_backing_bytes: u64,
	max_expanded_bytes: u64,
	max_delta_depth: usize,
	max_pack_count: usize,
	max_index_bytes: u64,
}

impl ObjectReadLimits {
	/// Define every independently enforced limit for one object read.
	pub const fn new(
		max_object_bytes: u64,
		max_backing_bytes: u64,
		max_expanded_bytes: u64,
		max_delta_depth: usize,
		max_pack_count: usize,
		max_index_bytes: u64,
	) -> Self {
		Self {
			max_object_bytes,
			max_backing_bytes,
			max_expanded_bytes,
			max_delta_depth,
			max_pack_count,
			max_index_bytes,
		}
	}

	/// Maximum returned object payload size.
	pub const fn max_object_bytes(&self) -> u64 {
		self.max_object_bytes
	}

	/// Maximum aggregate compressed loose or pack-entry bytes read.
	pub const fn max_backing_bytes(&self) -> u64 {
		self.max_backing_bytes
	}

	/// Maximum aggregate inflated and materialised bytes.
	pub const fn max_expanded_bytes(&self) -> u64 {
		self.max_expanded_bytes
	}

	/// Maximum number of deltas between a packed object and its base.
	pub const fn max_delta_depth(&self) -> usize {
		self.max_delta_depth
	}

	/// Maximum number of packfiles that may be considered while locating the object.
	pub const fn max_pack_count(&self) -> usize {
		self.max_pack_count
	}

	/// Maximum aggregate encoded `.idx` bytes read while locating the object.
	pub const fn max_index_bytes(&self) -> u64 {
		self.max_index_bytes
	}

	pub(crate) const fn max_listing_entries(&self) -> usize {
		self.max_pack_count.saturating_mul(4).saturating_add(4)
	}

	pub(crate) const fn max_listing_bytes(&self) -> u64 {
		(self.max_listing_entries() as u64).saturating_mul(256)
	}
}
