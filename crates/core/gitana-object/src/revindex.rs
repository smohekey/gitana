//! The multi-pack-index reverse index — the object order git's reachability bitmaps address bits
//! by, stored in the MIDX's `RIDX` chunk.
//!
//! A MIDX lists its objects in lexical (id-sorted) order — the `OIDL` chunk, the order
//! [`crate::MultiPackIndex::lookup`] binary-searches. Bitmaps, however, number objects in *pack
//! order*: the preferred pack's objects first (ascending offset), then every other pack in
//! ascending pack id (each ascending offset). The `RIDX` chunk records this order as
//! `order[bitmap_position] = lexical_index`, so a set bit resolves to an object id via the `OIDL`
//! ids. [`pack_order`] computes it (git's `midx_pack_order`); the MIDX codec reads and writes it.

/// Compute a MIDX's bitmap object order (the `RIDX` chunk): `order[bitmap_position]` is the lexical
/// index (into the id-sorted `OIDL`) of the object at that bitmap position.
///
/// `locations` gives each object's `(pack_id, offset)` in lexical order (so `locations[i]` is the
/// `i`-th id-sorted object). Objects are ordered by the preferred pack first, then remaining packs
/// in ascending pack id, each pack's objects by ascending offset — matching git.
pub fn pack_order(locations: &[(u32, u64)], preferred_pack: u32) -> Vec<u32> {
	let mut order: Vec<u32> = (0..locations.len() as u32).collect();
	order.sort_by_key(|&i| {
		let (pack, offset) = locations[i as usize];
		// The preferred pack sorts before all others (`false` < `true`); the rest by pack id, then
		// every object by ascending offset. Offsets are unique within a pack, so the order is total.
		(pack != preferred_pack, pack, offset)
	});
	order
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn orders_preferred_pack_first_then_by_pack_then_offset() {
		// Three packs, lexical order deliberately unrelated to pack order.
		let locations = [
			(0, 100), // 0
			(2, 50),  // 1
			(1, 30),  // 2
			(0, 20),  // 3
			(1, 10),  // 4
			(2, 90),  // 5
		];
		// Preferred pack 1 first (by offset: idx 4 @10, idx 2 @30), then pack 0 (idx 3 @20, idx 0
		// @100), then pack 2 (idx 1 @50, idx 5 @90).
		assert_eq!(pack_order(&locations, 1), vec![4, 2, 3, 0, 1, 5]);
		// A different preferred pack reshuffles only which group leads.
		assert_eq!(pack_order(&locations, 2), vec![1, 5, 3, 0, 4, 2]);
		// Preferred pack 0 leads; remaining packs 1 then 2.
		assert_eq!(pack_order(&locations, 0), vec![3, 0, 4, 2, 1, 5]);
	}

	#[test]
	fn empty_is_empty() {
		assert!(pack_order(&[], 0).is_empty());
	}
}
