//! Myers O(ND) line diff (the algorithm git uses by default), producing an edit
//! script over two line sequences. See "An O(ND) Difference Algorithm and Its
//! Variations", Eugene W. Myers, 1986.

/// One step in the edit script aligning the old and new line sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
	/// A line present in both, at old index `a` and new index `b`.
	Equal { a: usize, b: usize },
	/// A line removed from the old sequence at index `a`.
	Delete { a: usize },
	/// A line added to the new sequence at index `b`.
	Insert { b: usize },
}

/// Compute the shortest edit script transforming `a` into `b`.
pub fn diff<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Edit> {
	// Two empty sequences have no edits — and the Myers arrays below are sized `2*max+1`, which is 1
	// when both are empty, so the `v[k+1+offset]` probe at `d = 0` would index out of bounds and panic.
	if a.is_empty() && b.is_empty() {
		return Vec::new();
	}
	let n = a.len() as isize;
	let m = b.len() as isize;
	let max = (n + m) as usize;
	let offset = max as isize; // shift so k in [-max, max] maps to a Vec index
	let width = 2 * max + 1;
	let mut v = vec![0isize; width];
	let mut trace: Vec<Vec<isize>> = Vec::new();

	let mut depth = 0usize;
	'search: for d in 0..=max as isize {
		trace.push(v.clone());
		let mut k = -d;
		while k <= d {
			// Choose to extend downward (insert) or rightward (delete).
			let mut x =
				if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) {
					v[(k + 1 + offset) as usize]
				} else {
					v[(k - 1 + offset) as usize] + 1
				};
			let mut y = x - k;
			while x < n && y < m && a[x as usize] == b[y as usize] {
				x += 1;
				y += 1;
			}
			v[(k + offset) as usize] = x;
			if x >= n && y >= m {
				depth = d as usize;
				break 'search;
			}
			k += 2;
		}
	}

	backtrack(&trace, depth, n, m, offset)
}

/// Walk the saved per-depth frontiers backwards to recover the edit script.
fn backtrack(trace: &[Vec<isize>], depth: usize, n: isize, m: isize, offset: isize) -> Vec<Edit> {
	let mut edits = Vec::new();
	let mut x = n;
	let mut y = m;
	for d in (0..=depth).rev() {
		let v = &trace[d];
		let k = x - y;
		let prev_k = if k == -(d as isize)
			|| (k != d as isize && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize])
		{
			k + 1
		} else {
			k - 1
		};
		let prev_x = v[(prev_k + offset) as usize];
		let prev_y = prev_x - prev_k;

		while x > prev_x && y > prev_y {
			edits.push(Edit::Equal {
				a: (x - 1) as usize,
				b: (y - 1) as usize,
			});
			x -= 1;
			y -= 1;
		}
		if d > 0 {
			if x == prev_x {
				edits.push(Edit::Insert { b: prev_y as usize });
			} else {
				edits.push(Edit::Delete { a: prev_x as usize });
			}
		}
		x = prev_x;
		y = prev_y;
	}
	edits.reverse();
	edits
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn both_empty_yields_no_edits() {
		// Two empty sequences must not panic (the Myers arrays are width 1 here) and have no edits.
		let empty: [&str; 0] = [];
		assert!(diff(&empty, &empty).is_empty());
	}

	#[test]
	fn identical_is_all_equal() {
		let a = ["x", "y", "z"];
		let edits = diff(&a, &a);
		assert!(edits.iter().all(|e| matches!(e, Edit::Equal { .. })));
		assert_eq!(edits.len(), 3);
	}

	#[test]
	fn replace_middle_line() {
		let a = ["a", "b", "c"];
		let b = ["a", "x", "c"];
		let edits = diff(&a, &b);
		assert!(edits.contains(&Edit::Delete { a: 1 }));
		assert!(edits.contains(&Edit::Insert { b: 1 }));
		// First and last lines are kept.
		assert_eq!(edits.first(), Some(&Edit::Equal { a: 0, b: 0 }));
		assert_eq!(edits.last(), Some(&Edit::Equal { a: 2, b: 2 }));
	}

	#[test]
	fn pure_insertion_into_empty() {
		let a: [&str; 0] = [];
		let b = ["a", "b"];
		let edits = diff(&a, &b);
		assert_eq!(edits, vec![Edit::Insert { b: 0 }, Edit::Insert { b: 1 }]);
	}
}
