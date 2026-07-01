use std::fmt;

use crate::ObjectError;

/// A git author/committer/tagger identity and timestamp.
///
/// Formats as `Name <email> <unix-seconds> <±hhmm>` — git's exact identity line, used when
/// constructing commits and tags. The parsers keep the raw line verbatim in the object for byte-exact
/// round-tripping; this type is the structured form. The [`TzOffset`] is preserved as written, so even
/// a non-canonical offset re-formats verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
	/// Display name.
	pub name: String,
	/// Email address (without angle brackets).
	pub email: String,
	/// Unix timestamp in seconds.
	pub seconds: i64,
	/// Timezone offset, preserved exactly as written.
	pub offset: TzOffset,
}

/// A git timezone offset (`±hhmm`), kept as written — including a non-canonical minutes field (`≥ 60`)
/// or the `-0000` "unknown timezone" marker — so identity lines round-trip verbatim. Use
/// [`TzOffset::total_minutes`] for date arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TzOffset {
	/// `-` (as opposed to `+`) — kept separate so `-0000` differs from `+0000`.
	pub negative: bool,
	/// The `hh` field, as written (may exceed 24 in a non-canonical offset).
	pub hours: u8,
	/// The `mm` field, as written (may exceed 59 in a non-canonical offset).
	pub minutes: u8,
}

impl TzOffset {
	/// Total minutes east of UTC (negative for west), for date arithmetic. A non-canonical `mm ≥ 60`
	/// is folded into hours here, though it is preserved verbatim by [`fmt::Display`].
	pub fn total_minutes(&self) -> i32 {
		let magnitude = self.hours as i32 * 60 + self.minutes as i32;
		if self.negative { -magnitude } else { magnitude }
	}
}

impl Signature {
	/// Parse a git identity line: `Name <email> seconds ±hhmm`.
	pub fn parse(line: &str) -> Result<Self, ObjectError> {
		// The email is the final `<...>` pair — a name may itself contain angle brackets — so scan
		// from the end, the way git splits identity lines.
		let email_end = line.rfind('>').ok_or(ObjectError::MalformedHeader)?;
		let email_start = line[..email_end]
			.rfind('<')
			.ok_or(ObjectError::MalformedHeader)?;

		let name = line[..email_start].trim_end().to_owned();
		let email = line[email_start + 1..email_end].to_owned();

		let mut rest = line[email_end + 1..].split_whitespace();
		let seconds = rest
			.next()
			.and_then(|s| s.parse().ok())
			.ok_or(ObjectError::MalformedHeader)?;
		let offset = parse_offset(rest.next().ok_or(ObjectError::MalformedHeader)?)?;

		Ok(Signature {
			name,
			email,
			seconds,
			offset,
		})
	}

	/// Total timezone minutes east of UTC (negative for west).
	pub fn offset_minutes(&self) -> i32 {
		self.offset.total_minutes()
	}

	/// The timestamp in git's `iso` format — `YYYY-MM-DD HH:MM:SS ±hhmm` — in the signature's own
	/// timezone. The time is computed from the offset; the `±hhmm` label is preserved as written.
	pub fn iso_date(&self) -> String {
		let local = self.seconds + self.offset_minutes() as i64 * 60;
		let (days, rem) = (local.div_euclid(86400), local.rem_euclid(86400));
		let (year, month, day) = civil_from_days(days);
		let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
		format!(
			"{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}:{ss:02} {}",
			self.offset
		)
	}
}

impl fmt::Display for Signature {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"{} <{}> {} {}",
			self.name, self.email, self.seconds, self.offset
		)
	}
}

impl fmt::Display for TzOffset {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let sign = if self.negative { '-' } else { '+' };
		write!(f, "{sign}{:02}{:02}", self.hours, self.minutes)
	}
}

fn parse_offset(tz: &str) -> Result<TzOffset, ObjectError> {
	// ±hhmm — a sign byte then four ASCII digits. Validate up front, then read the digits by byte so
	// a 5-byte token holding a multi-byte char cannot panic a str slice.
	let bytes = tz.as_bytes();
	if bytes.len() != 5 || !bytes[1..].iter().all(u8::is_ascii_digit) {
		return Err(ObjectError::MalformedHeader);
	}
	let negative = match bytes[0] {
		b'+' => false,
		b'-' => true,
		_ => return Err(ObjectError::MalformedHeader),
	};
	let hours = (bytes[1] - b'0') * 10 + (bytes[2] - b'0');
	let minutes = (bytes[3] - b'0') * 10 + (bytes[4] - b'0');
	Ok(TzOffset {
		negative,
		hours,
		minutes,
	})
}

/// Civil date `(year, month, day)` from a count of days since the Unix epoch (Howard Hinnant's
/// algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
	let z = z + 719468;
	let era = if z >= 0 { z } else { z - 146096 } / 146097;
	let doe = z - era * 146097;
	let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
	let year = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
	let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
	(if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn round_trips_git_identity_lines() {
		for line in [
			"A U Thor <author@example.com> 1700000000 +1000",
			"C O Mitter <committer@example.com> 1700000005 -0500",
			"x <y@z> 0 +0000",
		] {
			let signature = Signature::parse(line).expect("parse");
			assert_eq!(signature.to_string(), line, "must re-format verbatim");
		}
	}

	#[test]
	fn parses_email_as_the_final_bracket_pair() {
		// A name that itself contains angle brackets: the email is the last `<...>` pair.
		let sig = Signature::parse("A <x> <a@b> 1700000000 +0000").unwrap();
		assert_eq!(sig.name, "A <x>");
		assert_eq!(sig.email, "a@b");
		assert_eq!(sig.seconds, 1700000000);
		assert_eq!(sig.to_string(), "A <x> <a@b> 1700000000 +0000");
	}

	#[test]
	fn parses_offset_fields() {
		let sig = Signature::parse("A <a@b> 1700000000 +1000").unwrap();
		assert_eq!(sig.offset_minutes(), 600);
		let sig = Signature::parse("A <a@b> 1700000000 -0530").unwrap();
		assert_eq!(sig.offset_minutes(), -330);
	}

	#[test]
	fn preserves_non_canonical_offsets() {
		// git accepts (but never writes) offsets like +1260, or the -0000 "unknown timezone" marker;
		// the `±hhmm` label must round-trip verbatim even though `offset_minutes` normalizes for math.
		for line in ["A <a@b> 0 +1260", "A <a@b> 0 -0000"] {
			assert_eq!(Signature::parse(line).unwrap().to_string(), line);
		}
		let sig = Signature::parse("A <a@b> 0 +1260").unwrap();
		assert_eq!(sig.offset_minutes(), 780);
		// The time is computed from the normalized offset; the label stays as written.
		assert_eq!(sig.iso_date(), "1970-01-01 13:00:00 +1260");
	}

	#[test]
	fn rejects_a_non_ascii_timezone_without_panicking() {
		// The tz token is 5 bytes but holds a multi-byte char; the offset parser must reject it, not
		// panic reading a non-char boundary.
		assert!(Signature::parse("T <t@e> 0 +aé1").is_err());
	}

	#[test]
	fn iso_date_renders_in_the_local_timezone() {
		let at = |offset| Signature {
			name: "x".to_owned(),
			email: "y".to_owned(),
			seconds: 0,
			offset,
		};
		let tz = |negative, hours, minutes| TzOffset {
			negative,
			hours,
			minutes,
		};
		// The Unix epoch, shown in three timezones.
		assert_eq!(at(tz(false, 0, 0)).iso_date(), "1970-01-01 00:00:00 +0000");
		assert_eq!(at(tz(false, 10, 0)).iso_date(), "1970-01-01 10:00:00 +1000");
		assert_eq!(at(tz(true, 5, 0)).iso_date(), "1969-12-31 19:00:00 -0500");
	}
}
