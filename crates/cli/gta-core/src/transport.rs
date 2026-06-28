//! Driving a Git Smart HTTP remote: origin config, ref discovery, and pack
//! download/upload.
//!
//! The wire codec lives in `gitana-git-http` (protocol v0 client helpers); this
//! module pairs it with `gta`'s HTTP client and local repository.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gitana_config::GitConfig;
use gitana_git_http::{
	Advertised, build_upload_pack_request, parse_advertisement, parse_upload_pack_response,
};
use gitana_object::ObjectId;

use crate::remote::{http_get, http_post};
use crate::repo::LocalRepository;

const UPLOAD_PACK_REQUEST: &str = "application/x-git-upload-pack-request";
/// Content type for a `git-receive-pack` request body.
pub const RECEIVE_PACK_REQUEST: &str = "application/x-git-receive-pack-request";

const ORIGIN_FETCH_REFSPEC: &str = "+refs/heads/*:refs/remotes/origin/*";

/// The configured origin remote. This is the base Smart HTTP URL, e.g.
/// `https://example.com/acme/project.git`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
	/// The repository base URL.
	pub url: String,
}

impl Origin {
	/// Validate and normalise a Smart HTTP remote URL.
	pub fn parse(url: &str) -> Result<Self> {
		let trimmed = url.trim_end_matches('/');
		if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
			bail!("only http(s) Smart HTTP remotes are supported");
		}
		Ok(Self {
			url: trimmed.to_owned(),
		})
	}

	/// The default checkout directory name for this remote URL.
	pub fn directory_name(&self) -> String {
		let without_query = self
			.url
			.split(['?', '#'])
			.next()
			.unwrap_or(self.url.as_str());
		let last = without_query
			.rsplit('/')
			.find(|segment| !segment.is_empty())
			.unwrap_or("repository");
		let name = last.strip_suffix(".git").unwrap_or(last);
		if name.is_empty() {
			"repository".to_owned()
		} else {
			name.to_owned()
		}
	}

	/// Persist the origin as a standard git remote in `.git/config`.
	pub fn save(&self, git_dir: &Path) -> Result<()> {
		let path = git_dir.join("config");
		let text =
			std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
		let mut config =
			GitConfig::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
		config.set("remote", Some("origin"), "url", &self.url);
		config.set("remote", Some("origin"), "fetch", ORIGIN_FETCH_REFSPEC);
		std::fs::write(&path, config.render())
			.with_context(|| format!("writing {}", path.display()))?;
		Ok(())
	}

	/// Load `remote.origin.url` from `.git/config`.
	pub fn load(git_dir: &Path) -> Result<Self> {
		let path = git_dir.join("config");
		let text =
			std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
		let config = GitConfig::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
		let url = config
			.get_string("remote", Some("origin"), "url")
			.context("no remote.origin.url configured")?;
		Self::parse(url)
	}

	fn info_refs(&self, service: &str) -> String {
		format!("{}/info/refs?service={service}", self.url)
	}

	/// The `git-upload-pack` (fetch) endpoint.
	pub fn upload_pack(&self) -> String {
		format!("{}/git-upload-pack", self.url)
	}

	/// The `git-receive-pack` (push) endpoint.
	pub fn receive_pack(&self) -> String {
		format!("{}/git-receive-pack", self.url)
	}
}

/// Fetch the `upload-pack` (fetch-side) ref advertisement.
pub async fn discover_upload(origin: &Origin) -> Result<Advertised> {
	let body = http_get(&origin.info_refs("git-upload-pack")).await?;
	Ok(parse_advertisement(&body)?)
}

/// Fetch the `receive-pack` (push-side) ref advertisement.
pub async fn discover_receive(origin: &Origin) -> Result<Advertised> {
	let body = http_get(&origin.info_refs("git-receive-pack")).await?;
	Ok(parse_advertisement(&body)?)
}

/// Download the objects reachable from `wants` but not `haves` into `repo`.
pub async fn fetch_pack(
	origin: &Origin,
	repo: &LocalRepository,
	wants: &[ObjectId],
	haves: &[ObjectId],
) -> Result<()> {
	if wants.is_empty() {
		return Ok(());
	}
	let request = build_upload_pack_request(wants, haves);
	let response = http_post(&origin.upload_pack(), UPLOAD_PACK_REQUEST, request).await?;
	let pack = parse_upload_pack_response(&response)?;
	// Skip an empty pack (server had nothing new). The 12-byte header carries the count.
	if pack.len() >= 12 && pack_object_count(&pack) > 0 {
		repo
			.objects()
			.write_pack(&pack)
			.await
			.context("storing fetched pack")?;
	}
	Ok(())
}

/// The object count from a packfile's v2 header.
fn pack_object_count(pack: &[u8]) -> u32 {
	u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]])
}

/// The unique object ids advertised across all refs (fetch wants / push haves).
pub fn advertised_oids(advertised: &Advertised) -> Vec<ObjectId> {
	let mut oids: Vec<ObjectId> = advertised.refs.iter().map(|(_, oid)| *oid).collect();
	oids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
	oids.dedup();
	oids
}

/// The unique tips of every local ref, sent as fetch `have`s so the server omits
/// objects we already hold.
pub async fn local_haves(repo: &LocalRepository) -> Result<Vec<ObjectId>> {
	let mut oids: Vec<ObjectId> = repo
		.refs()
		.list("refs/")
		.await?
		.into_iter()
		.map(|(_, oid)| oid)
		.collect();
	oids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
	oids.dedup();
	Ok(oids)
}

#[cfg(test)]
mod tests {
	use std::fs;

	use tempfile::TempDir;

	use super::*;

	#[test]
	fn origin_directory_name_matches_git_clone_convention() {
		assert_eq!(
			Origin::parse("https://example.com/acme/app.git")
				.unwrap()
				.directory_name(),
			"app"
		);
		assert_eq!(
			Origin::parse("http://example.com/acme/app")
				.unwrap()
				.directory_name(),
			"app"
		);
	}

	#[test]
	fn origin_round_trips_through_git_config() {
		let dir = TempDir::new().unwrap();
		let git_dir = dir.path().join(".git");
		fs::create_dir_all(&git_dir).unwrap();
		fs::write(
			git_dir.join("config"),
			"[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha256\n",
		)
		.unwrap();

		let origin = Origin::parse("https://example.com/acme/app.git").unwrap();
		origin.save(&git_dir).unwrap();

		assert_eq!(Origin::load(&git_dir).unwrap(), origin);
		let config = fs::read_to_string(git_dir.join("config")).unwrap();
		assert!(config.contains("[remote \"origin\"]"));
		assert!(config.contains("url = https://example.com/acme/app.git"));
		assert!(config.contains("fetch = +refs/heads/*:refs/remotes/origin/*"));
	}
}
