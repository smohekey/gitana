use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;

use anyhow::Result;
use gitana_object::{ObjectId, ObjectKind, parse_commit, parse_tag, parse_tree};
use gitana_worktree::FileDiff;

use crate::commands::diff;
use crate::repo::{self, LocalRepository};

/// Show an object: a commit (header plus its diff against the first parent), an annotated tag
/// (header plus the object it points at), a tree (its entries), or a blob (its raw bytes).
/// Defaults to `HEAD`.
pub async fn run(cwd: &Path, object: Option<String>) -> Result<()> {
	let (repo, oid) = repo::resolve_object(cwd, object.as_deref().unwrap_or("HEAD")).await?;
	show_object(&repo, oid).await
}

/// Display the object `oid` according to its kind (boxed so a tag can recurse into its target).
fn show_object<'a>(
	repo: &'a LocalRepository,
	oid: ObjectId,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
	Box::pin(async move {
		let (kind, payload) = repo.objects().read_object(&oid).await?;
		match kind {
			ObjectKind::Commit => show_commit(repo, oid, &payload).await,
			ObjectKind::Tag => show_tag(repo, &payload).await,
			ObjectKind::Tree => show_tree(oid, &payload),
			ObjectKind::Blob => Ok(std::io::stdout().write_all(&payload)?),
		}
	})
}

async fn show_commit(repo: &LocalRepository, oid: ObjectId, payload: &[u8]) -> Result<()> {
	let commit = parse_commit(payload)?;
	let mut out = Vec::new();
	out.extend_from_slice(format!("commit {oid}\n").as_bytes());
	let (ident, date) = split_signature(&commit.author);
	out.extend_from_slice(format!("Author: {ident}\nDate:   {date}\n\n").as_bytes());
	for line in commit.message.lines() {
		out.extend_from_slice(format!("    {line}\n").as_bytes());
	}
	out.push(b'\n');

	// Diff the first parent's tree against this commit's tree (the empty tree for a root commit).
	let old = match commit.parents.first() {
		Some(parent) => tree_map(repo, repo.commit_tree(*parent).await?).await?,
		None => BTreeMap::new(),
	};
	let new = tree_map(repo, commit.tree).await?;
	for file in tree_diff(repo, &old, &new).await? {
		diff::format_file(&mut out, &file);
	}
	std::io::stdout().write_all(&out)?;
	Ok(())
}

async fn show_tag(repo: &LocalRepository, payload: &[u8]) -> Result<()> {
	let tag = parse_tag(payload)?;
	let mut out = Vec::new();
	out.extend_from_slice(format!("tag {}\n", tag.name).as_bytes());
	if let Some(tagger) = &tag.tagger {
		let (ident, date) = split_signature(tagger);
		out.extend_from_slice(format!("Tagger: {ident}\nDate:   {date}\n").as_bytes());
	}
	out.push(b'\n');
	for line in tag.message.lines() {
		out.extend_from_slice(format!("{line}\n").as_bytes());
	}
	out.push(b'\n');
	std::io::stdout().write_all(&out)?;

	// Then show the object the tag points at (commonly a commit).
	show_object(repo, tag.object).await
}

fn show_tree(oid: ObjectId, payload: &[u8]) -> Result<()> {
	let mut out = format!("tree {oid}\n\n");
	for entry in parse_tree(payload)? {
		out.push_str(&entry.name);
		out.push('\n');
	}
	print!("{out}");
	Ok(())
}

/// A tree flattened to `path -> (mode, oid)`, dropping gitlinks (submodule entries), which have
/// no blob to diff.
async fn tree_map(
	repo: &LocalRepository,
	tree: ObjectId,
) -> Result<BTreeMap<String, (String, ObjectId)>> {
	Ok(
		repo
			.read_tree(tree)
			.await?
			.into_iter()
			.filter(|(_, mode, _)| mode != "160000")
			.map(|(path, mode, oid)| (path, (mode, oid)))
			.collect(),
	)
}

/// The added, deleted, and modified paths between two flattened trees, with their blob content,
/// ready for the unified-diff formatter. Paths are sorted (the maps are ordered).
async fn tree_diff(
	repo: &LocalRepository,
	old: &BTreeMap<String, (String, ObjectId)>,
	new: &BTreeMap<String, (String, ObjectId)>,
) -> Result<Vec<FileDiff>> {
	let mut diffs = Vec::new();
	for (path, (omode, ooid)) in old {
		match new.get(path) {
			Some((nmode, noid)) if nmode == omode && noid == ooid => {}
			Some((nmode, noid)) => diffs.push(FileDiff {
				path: path.clone(),
				old: Some((repo.read_blob(*ooid).await?, parse_mode(omode))),
				new: Some((repo.read_blob(*noid).await?, parse_mode(nmode))),
			}),
			None => diffs.push(FileDiff {
				path: path.clone(),
				old: Some((repo.read_blob(*ooid).await?, parse_mode(omode))),
				new: None,
			}),
		}
	}
	for (path, (nmode, noid)) in new {
		if !old.contains_key(path) {
			diffs.push(FileDiff {
				path: path.clone(),
				old: None,
				new: Some((repo.read_blob(*noid).await?, parse_mode(nmode))),
			});
		}
	}
	diffs.sort_by(|a, b| a.path.cmp(&b.path));
	Ok(diffs)
}

fn parse_mode(mode: &str) -> u32 {
	u32::from_str_radix(mode, 8).unwrap_or(0o100644)
}

/// Split a git signature (`Name <email> <seconds> <±hhmm>`) into the identity (`Name <email>`)
/// and a rendered date, falling back to the raw trailer if it cannot be parsed.
fn split_signature(signature: &str) -> (&str, String) {
	let Some(angle) = signature.rfind('>') else {
		return (signature, String::new());
	};
	let (ident, trailer) = signature.split_at(angle + 1);
	let ident = ident.trim();
	let trailer = trailer.trim();
	let mut parts = trailer.split_whitespace();
	if let (Some(secs), Some(tz)) = (parts.next(), parts.next())
		&& let Ok(secs) = secs.parse::<i64>()
		&& let Some(date) = format_date(secs, tz)
	{
		return (ident, date);
	}
	(ident, trailer.to_owned())
}

/// Render `secs` (Unix time) in the timezone `tz` (`±hhmm`) as `YYYY-MM-DD HH:MM:SS ±hhmm`.
fn format_date(secs: i64, tz: &str) -> Option<String> {
	let sign = match tz.as_bytes().first()? {
		b'+' => 1,
		b'-' => -1,
		_ => return None,
	};
	let digits = &tz[1..];
	if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	let offset =
		sign * (digits[..2].parse::<i64>().ok()? * 3600 + digits[2..].parse::<i64>().ok()? * 60);
	let local = secs + offset;
	let (days, rem) = (local.div_euclid(86400), local.rem_euclid(86400));
	let (year, month, day) = civil_from_days(days);
	let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
	Some(format!(
		"{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}:{ss:02} {tz}"
	))
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
