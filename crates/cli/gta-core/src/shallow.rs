//! Building a shallow-fetch [`Deepen`] directive from `clone`/`fetch` command-line flags, shared by
//! both the `gta` and `gta-mcp` surfaces.

use anyhow::{Context, Result, bail};
use gitana_porcelain::Deepen;

/// Assemble the deepen directive from the shallow flags `--depth`, `--shallow-since`, and (repeatable)
/// `--shallow-exclude`. An all-absent set yields an empty [`Deepen`], which requests a normal (complete)
/// fetch.
pub fn build_deepen(
	depth: Option<u32>,
	shallow_since: Option<&str>,
	shallow_exclude: Vec<String>,
) -> Result<Deepen> {
	// git requires a positive depth; `deepen 0` is rejected by the server, so fail before the clone
	// initialises anything and leaves a stray directory behind.
	if depth == Some(0) {
		bail!("--depth must be a positive number of commits");
	}
	// git upload-pack rejects `deepen` together with `deepen-since`/`deepen-not` ("deepen and
	// deepen-since (or deepen-not) cannot be used together"), so refuse the combination up front rather
	// than initialise the clone and fail remotely. (`--shallow-since` + `--shallow-exclude` is allowed.)
	if depth.is_some() && (shallow_since.is_some() || !shallow_exclude.is_empty()) {
		bail!("--depth cannot be combined with --shallow-since or --shallow-exclude");
	}
	let since = shallow_since.map(parse_since).transpose()?;
	Ok(Deepen {
		depth,
		since,
		not: shallow_exclude,
		// The gitana client only ever does absolute-depth shallow clones/fetches.
		relative: false,
	})
}

/// Parse a `--shallow-since` value to a Unix timestamp (seconds). Accepts a bare Unix timestamp (all
/// digits, optionally negative) or an ISO-8601 **UTC** date/time: `YYYY-MM-DD`, optionally followed by
/// `T` or a space and `HH:MM[:SS]`, with an optional trailing `Z`. git's relative forms
/// (`"2 weeks ago"`) are not supported.
pub fn parse_since(value: &str) -> Result<i64> {
	let value = value.trim();
	// A bare Unix timestamp passes through unchanged.
	if let Ok(timestamp) = value.parse::<i64>() {
		return Ok(timestamp);
	}
	parse_iso_utc(value).with_context(|| {
		format!(
			"unrecognized --shallow-since date {value:?} (use a Unix timestamp or an ISO-8601 UTC date \
			 like 2020-01-31 or 2020-01-31T14:00:00Z)"
		)
	})
}

/// Parse an ISO-8601 UTC date/time to a Unix timestamp. Time defaults to `00:00:00`.
fn parse_iso_utc(value: &str) -> Result<i64> {
	let value = value.strip_suffix('Z').unwrap_or(value);
	let (date, time) = match value.split_once(['T', ' ']) {
		Some((date, time)) => (date, Some(time)),
		None => (value, None),
	};

	let mut date_parts = date.split('-');
	let year: i64 = field(date_parts.next(), "year")?;
	let month: i64 = field(date_parts.next(), "month")?;
	let day: i64 = field(date_parts.next(), "day")?;
	if date_parts.next().is_some() {
		bail!("too many '-'-separated date components");
	}
	if !(1..=12).contains(&month) {
		bail!("month out of range");
	}
	// Validate the day against the actual month length (leap years included) so a typo like
	// `2020-02-31` is rejected rather than silently normalised to a different day.
	if !(1..=days_in_month(year, month)).contains(&day) {
		bail!("day out of range for the month");
	}

	let (hour, minute, second) = match time {
		None => (0, 0, 0),
		Some(time) => {
			let mut parts = time.split(':');
			let hour: i64 = field(parts.next(), "hour")?;
			let minute: i64 = field(parts.next(), "minute")?;
			let second: i64 = match parts.next() {
				Some(text) => text.parse().context("bad second")?,
				None => 0,
			};
			if parts.next().is_some() {
				bail!("too many ':'-separated time components");
			}
			(hour, minute, second)
		}
	};
	// A leap second (`:60`) is tolerated; hour/minute are strict.
	if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..=60).contains(&second) {
		bail!("time component out of range");
	}

	Ok(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Parse a required, present numeric date/time field.
fn field(part: Option<&str>, name: &str) -> Result<i64> {
	part
		.with_context(|| format!("missing {name}"))?
		.parse()
		.with_context(|| format!("bad {name}"))
}

/// The number of days in `month` (1..=12) of `year`, honoring leap years.
fn days_in_month(year: i64, month: i64) -> i64 {
	match month {
		1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
		4 | 6 | 9 | 11 => 30,
		2 if is_leap_year(year) => 29,
		2 => 28,
		_ => 0,
	}
}

/// Whether `year` is a Gregorian leap year.
fn is_leap_year(year: i64) -> bool {
	(year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian civil date (Howard Hinnant's
/// `days_from_civil`, exact for the full range). `month` is 1..=12, `day` 1..=31.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
	let year = if month <= 2 { year - 1 } else { year };
	let era = (if year >= 0 { year } else { year - 399 }) / 400;
	let year_of_era = year - era * 400; // [0, 399]
	let month_of = if month > 2 { month - 3 } else { month + 9 };
	let day_of_year = (153 * month_of + 2) / 5 + day - 1; // [0, 365]
	let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
	era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_unix_timestamp_verbatim() {
		assert_eq!(parse_since("1577836800").unwrap(), 1_577_836_800);
		assert_eq!(parse_since("0").unwrap(), 0);
		assert_eq!(parse_since("-1").unwrap(), -1);
	}

	#[test]
	fn parses_iso_utc_dates() {
		// 2020-01-01T00:00:00Z is 1577836800.
		assert_eq!(parse_since("2020-01-01").unwrap(), 1_577_836_800);
		assert_eq!(parse_since("2020-01-01T00:00:00Z").unwrap(), 1_577_836_800);
		assert_eq!(parse_since("2020-01-01 00:00:00").unwrap(), 1_577_836_800);
		// The epoch itself, and a known later instant (2021-12-31T23:59:59Z = 1640995199).
		assert_eq!(parse_since("1970-01-01").unwrap(), 0);
		assert_eq!(parse_since("2021-12-31T23:59:59Z").unwrap(), 1_640_995_199);
	}

	#[test]
	fn rejects_garbage_and_relative_dates() {
		assert!(parse_since("2 weeks ago").is_err());
		assert!(parse_since("2020-13-01").is_err());
		assert!(parse_since("2020-01-32").is_err());
		assert!(parse_since("2020-01-01T25:00:00").is_err());
		assert!(parse_since("not-a-date").is_err());
	}

	#[test]
	fn rejects_impossible_calendar_days() {
		// A day past the month's real length is a typo, not silently normalised to the next month.
		assert!(parse_since("2020-02-31").is_err());
		assert!(parse_since("2021-02-29").is_err()); // 2021 is not a leap year
		assert!(parse_since("2020-02-29").is_ok()); // 2020 is a leap year
		assert!(parse_since("2020-04-31").is_err()); // April has 30 days
		assert!(parse_since("2020-04-30").is_ok());
	}

	#[test]
	fn build_deepen_is_empty_when_no_flags() {
		let deepen = build_deepen(None, None, Vec::new()).unwrap();
		assert!(deepen.is_empty());
	}

	#[test]
	fn build_deepen_rejects_zero_depth() {
		assert!(build_deepen(Some(0), None, Vec::new()).is_err());
		assert!(build_deepen(Some(1), None, Vec::new()).is_ok());
	}

	#[test]
	fn build_deepen_rejects_depth_with_since_or_exclude() {
		// git upload-pack rejects `deepen` alongside `deepen-since`/`deepen-not`.
		assert!(build_deepen(Some(1), Some("2020-01-01"), Vec::new()).is_err());
		assert!(build_deepen(Some(1), None, vec!["main".to_owned()]).is_err());
		// But since + exclude together is allowed.
		assert!(build_deepen(None, Some("2020-01-01"), vec!["main".to_owned()]).is_ok());
	}

	#[test]
	fn build_deepen_carries_flags() {
		// `--depth` alone.
		let deepen = build_deepen(Some(1), None, Vec::new()).unwrap();
		assert_eq!(deepen.depth, Some(1));
		// `--shallow-since` + `--shallow-exclude` (the allowed non-depth combination).
		let deepen = build_deepen(None, Some("2020-01-01"), vec!["v1".to_owned()]).unwrap();
		assert_eq!(deepen.since, Some(1_577_836_800));
		assert_eq!(deepen.not, vec!["v1".to_owned()]);
	}
}
