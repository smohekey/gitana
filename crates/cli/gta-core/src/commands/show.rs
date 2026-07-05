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
use gitana_worktree::diff_trees;

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

	// Diff the first parent's tree against this commit's tree (an empty left side for a root commit).
	let old_tree = match commit.parents.first() {
		Some(parent) => Some(repo.commit_tree(*parent).await?),
		None => None,
	};
	for file in diff_trees(repo, old_tree, commit.tree).await? {
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
	// A signed tag's armor block follows the message in git's `show` output.
	if let Some(signature) = &tag.signature {
		for line in signature.lines() {
			out.extend_from_slice(format!("{line}\n").as_bytes());
		}
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

/// The identity (`Name <email>`) and rendered date of a git signature line, falling back to the raw
/// line with no date if it cannot be parsed.
fn split_signature(signature: &str) -> (String, String) {
	match Signature::parse(signature) {
		Ok(sig) => (format!("{} <{}>", sig.name, sig.email), sig.iso_date()),
		Err(_) => (signature.to_owned(), String::new()),
	}
}
