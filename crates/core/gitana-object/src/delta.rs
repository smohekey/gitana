use crate::ObjectError;
use crate::loose::MAX_OBJECT_SIZE;

/// Apply a git delta to a base object, producing the target object.
///
/// The delta starts with the source and target sizes (little-endian base-128
/// varints), followed by copy and insert instructions. Copy instructions read a
/// run from `base`; insert instructions carry literal bytes. See
/// gitformat-pack(5).
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, ObjectError> {
	let mut cursor = 0;

	let source_size = read_size(delta, &mut cursor)?;
	if source_size != base.len() {
		return Err(ObjectError::MalformedDelta);
	}
	let target_size = read_size(delta, &mut cursor)?;
	if target_size as u64 > MAX_OBJECT_SIZE {
		return Err(ObjectError::TooLarge);
	}

	let mut out = Vec::with_capacity(target_size);
	while cursor < delta.len() {
		let cmd = delta[cursor];
		cursor += 1;
		if cmd & 0x80 != 0 {
			// Copy `size` bytes from `base` at `offset`.
			let mut offset = 0usize;
			for (i, mask) in [0x01, 0x02, 0x04, 0x08].into_iter().enumerate() {
				if cmd & mask != 0 {
					offset |= (next_byte(delta, &mut cursor)? as usize) << (8 * i);
				}
			}
			let mut size = 0usize;
			for (i, mask) in [0x10, 0x20, 0x40].into_iter().enumerate() {
				if cmd & mask != 0 {
					size |= (next_byte(delta, &mut cursor)? as usize) << (8 * i);
				}
			}
			if size == 0 {
				size = 0x10000;
			}
			let end = offset
				.checked_add(size)
				.ok_or(ObjectError::MalformedDelta)?;
			let run = base.get(offset..end).ok_or(ObjectError::MalformedDelta)?;
			out.extend_from_slice(run);
		} else if cmd != 0 {
			// Insert the next `cmd` literal bytes.
			let len = cmd as usize;
			let end = cursor.checked_add(len).ok_or(ObjectError::MalformedDelta)?;
			let run = delta.get(cursor..end).ok_or(ObjectError::MalformedDelta)?;
			out.extend_from_slice(run);
			cursor = end;
		} else {
			// A 0x00 command is reserved and invalid.
			return Err(ObjectError::MalformedDelta);
		}
		if out.len() > target_size {
			return Err(ObjectError::MalformedDelta);
		}
	}

	if out.len() != target_size {
		return Err(ObjectError::MalformedDelta);
	}
	Ok(out)
}

/// Read a little-endian base-128 varint (the delta size encoding).
fn read_size(delta: &[u8], cursor: &mut usize) -> Result<usize, ObjectError> {
	let mut size = 0usize;
	let mut shift = 0u32;
	loop {
		let byte = next_byte(delta, cursor)?;
		let value = (byte & 0x7f) as usize;
		size = size
			.checked_add(
				value
					.checked_shl(shift)
					.ok_or(ObjectError::MalformedDelta)?,
			)
			.ok_or(ObjectError::MalformedDelta)?;
		if byte & 0x80 == 0 {
			return Ok(size);
		}
		shift += 7;
	}
}

fn next_byte(data: &[u8], cursor: &mut usize) -> Result<u8, ObjectError> {
	let byte = *data.get(*cursor).ok_or(ObjectError::MalformedDelta)?;
	*cursor += 1;
	Ok(byte)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Build a delta that turns `base` into `target` using one insert and one copy,
	/// to exercise both instruction kinds.
	fn delta_copy_then_insert(
		base_len: usize,
		copy_off: usize,
		copy_len: usize,
		insert: &[u8],
	) -> Vec<u8> {
		let mut delta = Vec::new();
		// source size
		encode_size(&mut delta, base_len);
		// target size
		encode_size(&mut delta, copy_len + insert.len());
		// copy instruction: offset (1 byte) + size (1 byte)
		delta.push(0x80 | 0x01 | 0x10);
		delta.push(copy_off as u8);
		delta.push(copy_len as u8);
		// insert instruction
		delta.push(insert.len() as u8);
		delta.extend_from_slice(insert);
		delta
	}

	fn encode_size(out: &mut Vec<u8>, mut size: usize) {
		loop {
			let mut byte = (size & 0x7f) as u8;
			size >>= 7;
			if size != 0 {
				byte |= 0x80;
			}
			out.push(byte);
			if size == 0 {
				break;
			}
		}
	}

	#[test]
	fn applies_copy_and_insert() {
		let base = b"hello world";
		let delta = delta_copy_then_insert(base.len(), 0, 5, b"!!!"); // copy "hello" + "!!!"
		let out = apply_delta(base, &delta).expect("apply");
		assert_eq!(out, b"hello!!!");
	}

	#[test]
	fn rejects_copy_out_of_range() {
		let base = b"short";
		let delta = delta_copy_then_insert(base.len(), 3, 100, b""); // copy past the end
		assert!(matches!(
			apply_delta(base, &delta),
			Err(ObjectError::MalformedDelta)
		));
	}

	#[test]
	fn rejects_wrong_source_size() {
		let base = b"hello world";
		let mut delta = Vec::new();
		encode_size(&mut delta, 999); // wrong source size
		encode_size(&mut delta, 1);
		delta.push(1);
		delta.push(b'x');
		assert!(matches!(
			apply_delta(base, &delta),
			Err(ObjectError::MalformedDelta)
		));
	}
}
