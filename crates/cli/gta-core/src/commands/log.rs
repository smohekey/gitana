use std::path::Path;

use anyhow::Result;
use gitana_object::parse_commit;

use crate::repo;

/// Print the commit history of `HEAD`, newest first, one line each.
pub async fn run(cwd: &Path) -> Result<()> {
	let repo = repo::open_here(cwd)?;
	let Some(head) = repo.refs().resolve_head().await? else {
		return Ok(()); // unborn branch — no commits
	};
	for oid in repo.rev_list(&[head]).await? {
		let (_, payload) = repo.objects().read_object(&oid).await?;
		let subject = parse_commit(&payload)?
			.message
			.lines()
			.next()
			.unwrap_or("")
			.to_owned();
		println!("{oid} {subject}");
	}
	Ok(())
}
