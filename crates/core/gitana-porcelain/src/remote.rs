//! Remote composites over `gitana-remote`'s Smart-HTTP primitives — `clone` and `fetch` (with `pull`
//! and `push` to follow). Each returns data; the CLI adapter fetches the ref advertisement
//! (hash-agnostic) and dispatches the hash algorithm, then calls these generic over the file store.

use std::path::Path;

use anyhow::Result;
use gitana_file_store::FileStore;
use gitana_git_http::{Advertised, parse_advertisement};
use gitana_object::{HashAlgorithm, ObjectId};
use gitana_remote::Origin;
use gitana_repository::Repository;
use gitana_worktree::WorkTree;

/// The remote-tracking refs a [`fetch`] advanced (`(name, new oid)`).
pub struct FetchOutcome<H: HashAlgorithm> {
	pub updated: Vec<(String, ObjectId<H>)>,
}

/// Fetch every advertised branch from `origin` into `refs/remotes/origin/*`, downloading the objects
/// we do not already have. `advertisement` is the already-fetched `GET /info/refs` body; the working
/// tree is not touched.
pub async fn fetch<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
) -> Result<FetchOutcome<H>> {
	let advertised = parse_advertisement::<H>(advertisement)?;
	let haves = gitana_remote::local_haves(repo).await?;
	download(repo, origin, &advertised, &haves).await?;

	let mut updated = Vec::new();
	for (name, oid) in advertised.branches() {
		let short = name.strip_prefix("refs/heads/").unwrap_or(name);
		let tracking = format!("refs/remotes/origin/{short}");
		let current = repo.refs().resolve(&tracking).await?;
		if current != Some(oid) {
			repo.refs().update_ref(&tracking, oid, current).await?;
			updated.push((tracking, oid));
		}
	}
	Ok(FetchOutcome { updated })
}

/// Clone the advertised repository into `work_dir` (whose `.git` backs `repo`): initialise it (writing
/// a config matching `H`), download every advertised tip, recreate the refs and `HEAD`, save the
/// origin, and check out `HEAD`. `advertisement` is the already-fetched `GET /info/refs` body.
pub async fn clone<F: FileStore, H: HashAlgorithm>(
	repo: Repository<F, H>,
	origin: &Origin,
	advertisement: &[u8],
	work_dir: &Path,
) -> Result<()> {
	let git_dir = work_dir.join(".git");
	repo.init().await?;

	let advertised = parse_advertisement::<H>(advertisement)?;
	download(&repo, origin, &advertised, &[]).await?;

	// Recreate the refs and HEAD locally.
	for (name, oid) in &advertised.refs {
		if name.starts_with("refs/") {
			repo.refs().update_ref(name, *oid, None).await?;
		}
	}
	let head_target = advertised
		.head_target
		.clone()
		.unwrap_or_else(|| "refs/heads/main".to_owned());
	repo.refs().set_head_symbolic(&head_target).await?;
	origin.save(&git_dir)?;

	// Populate the working tree from HEAD (if the repo had any commits).
	if let Some(commit) = repo.refs().resolve_head().await? {
		let tree = repo.commit_tree(commit).await?;
		let worktree = WorkTree::new(repo, work_dir, git_dir);
		worktree.checkout(tree, true).await?;
	}
	Ok(())
}

/// Download the objects reachable from the advertised tips that `haves` do not already cover, writing
/// them into `repo`.
async fn download<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	origin: &Origin,
	advertised: &Advertised<H>,
	haves: &[ObjectId<H>],
) -> Result<()> {
	let wants = gitana_remote::advertised_oids(advertised);
	gitana_remote::fetch_pack(origin, repo, &wants, haves).await?;
	Ok(())
}
