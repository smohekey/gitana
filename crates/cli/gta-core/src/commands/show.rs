use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;

use crate::Backend;
use anyhow::Result;
use gitana_object::{
	HashAlgorithm, ObjectId, ObjectKind, Signature, parse_commit, parse_tag, parse_tree,
};
use gitana_repository::Repository;
use gitana_worktree::FileDiff;

use crate::commands::diff;
use crate::dispatch::{self, ObjectCommand};

/// Show an object: a commit (header plus its diff against the first parent), an annotated tag
/// (header plus the object it points at), a tree (its entries), or a blob (its raw bytes).
/// Defaults to `HEAD`.
pub async fn run(cwd: &Path, object: Option<String>) -> Result<()> {
	dispatch::on_object(cwd, object.as_deref().unwrap_or("HEAD"), Show).await
}

struct Show;

impl ObjectCommand for Show {
	async fn run<H: HashAlgorithm>(
		self,
		repo: Repository<Backend, H>,
		oid: ObjectId<H>,
	) -> Result<()> {
		show_object(&repo, oid).await
	}
}

/// Display the object `oid` according to its kind (boxed so a tag can recurse into its target).
fn show_object<'a, H: HashAlgorithm>(
	repo: &'a Repository<Backend, H>,
	oid: ObjectId<H>,
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

async fn show_commit<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	oid: ObjectId<H>,
	payload: &[u8],
) -> Result<()> {
	let commit = parse_commit::<H>(payload)?;
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

async fn show_tag<H: HashAlgorithm>(repo: &Repository<Backend, H>, payload: &[u8]) -> Result<()> {
	let tag = parse_tag::<H>(payload)?;
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

fn show_tree<H: HashAlgorithm>(oid: ObjectId<H>, payload: &[u8]) -> Result<()> {
	let mut out = format!("tree {oid}\n\n");
	for entry in parse_tree::<H>(payload)? {
		out.push_str(&entry.name);
		out.push('\n');
	}
	print!("{out}");
	Ok(())
}

/// A tree flattened to `path -> (mode, oid)`, dropping gitlinks (submodule entries), which have
/// no blob to diff.
async fn tree_map<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	tree: ObjectId<H>,
) -> Result<BTreeMap<String, (String, ObjectId<H>)>> {
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
async fn tree_diff<H: HashAlgorithm>(
	repo: &Repository<Backend, H>,
	old: &BTreeMap<String, (String, ObjectId<H>)>,
	new: &BTreeMap<String, (String, ObjectId<H>)>,
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

/// The identity (`Name <email>`) and rendered date of a git signature line, falling back to the raw
/// line with no date if it cannot be parsed.
fn split_signature(signature: &str) -> (String, String) {
	match Signature::parse(signature) {
		Ok(sig) => (format!("{} <{}>", sig.name, sig.email), sig.iso_date()),
		Err(_) => (signature.to_owned(), String::new()),
	}
}
