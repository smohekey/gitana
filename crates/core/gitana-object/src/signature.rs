use std::fmt;

use crate::ObjectError;

/// A git author/committer/tagger identity and timestamp.
///
/// Formats as `Name <email> <unix-seconds> <±hhmm>` — git's exact identity line,
/// used when constructing commits and tags. The parsers keep the raw line verbatim
/// for byte-exact round-tripping; this type is the structured form for creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
	/// Display name.
	pub name: String,
	/// Email address (without angle brackets).
	pub email: String,
	/// Unix timestamp in seconds.
	pub seconds: i64,
	/// Timezone offset in minutes east of UTC (e.g. +600 for `+1000`).
	pub offset_minutes: i32,
}

impl Signature {
	/// Parse a git identity line: `Name <email> seconds ±hhmm`.
	pub fn parse(line: &str) -> Result<Self, ObjectError> {
		let email_start = line.find('<').ok_or(ObjectError::MalformedHeader)?;
		let email_end = line[email_start..]
			.find('>')
			.map(|i| i + email_start)
			.ok_or(ObjectError::MalformedHeader)?;

		let name = line[..email_start].trim_end().to_owned();
		let email = line[email_start + 1..email_end].to_owned();

		let mut rest = line[email_end + 1..].split_whitespace();
		let seconds = rest
			.next()
			.and_then(|s| s.parse().ok())
			.ok_or(ObjectError::MalformedHeader)?;
		let offset_minutes = parse_offset(rest.next().ok_or(ObjectError::MalformedHeader)?)?;

		Ok(Signature {
			name,
			email,
			seconds,
			offset_minutes,
		})
	}
}

impl fmt::Display for Signature {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let sign = if self.offset_minutes < 0 { '-' } else { '+' };
		let abs = self.offset_minutes.unsigned_abs();
		write!(
			f,
			"{} <{}> {} {}{:02}{:02}",
			self.name,
			self.email,
			self.seconds,
			sign,
			abs / 60,
			abs % 60,
		)
	}
}

fn parse_offset(tz: &str) -> Result<i32, ObjectError> {
	// ±hhmm
	if tz.len() != 5 {
		return Err(ObjectError::MalformedHeader);
	}
	let sign = match tz.as_bytes()[0] {
		b'+' => 1,
		b'-' => -1,
		_ => return Err(ObjectError::MalformedHeader),
	};
	let hours: i32 = tz[1..3].parse().map_err(|_| ObjectError::MalformedHeader)?;
	let minutes: i32 = tz[3..5].parse().map_err(|_| ObjectError::MalformedHeader)?;
	Ok(sign * (hours * 60 + minutes))
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
	fn parses_offset_fields() {
		let sig = Signature::parse("A <a@b> 1700000000 +1000").unwrap();
		assert_eq!(sig.offset_minutes, 600);
		let sig = Signature::parse("A <a@b> 1700000000 -0530").unwrap();
		assert_eq!(sig.offset_minutes, -330);
	}
}
