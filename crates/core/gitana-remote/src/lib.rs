//! Driving a Git Smart HTTP remote: origin config, ref discovery, and pack
//! download/upload.
//!
//! The wire codec lives in `gitana-git-http` (protocol v0 client helpers); this
//! crate pairs it with an [`HttpTransport`] and a local repository. Object ids are
//! generic over the negotiated hash algorithm `H`: a caller first reads the remote's
//! advertised `object-format` ([`negotiated_kind`]) and then runs the rest under that
//! `H`. The transport is a capability the caller supplies — the native `ReqwestTransport` (behind
//! the default `reqwest-transport` feature) by default, or (on `wasm32-wasip2`) an in-guest
//! `wasi:http` client.

mod http_transport;
mod push_refspec;
mod refspec;
#[cfg(feature = "reqwest-transport")]
mod reqwest_transport;

pub use http_transport::HttpTransport;
pub use push_refspec::PushRefspec;
pub use refspec::Refspec;
#[cfg(feature = "reqwest-transport")]
pub use reqwest_transport::ReqwestTransport;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use gitana_config::GitConfig;
use gitana_file_store::FileStore;
use gitana_git_http::{
	Advertised, Deepen, build_upload_pack_request, parse_upload_pack_response, peek_object_format,
};
use gitana_object::{
	HashAlgorithm, HashKind, ObjectError, ObjectId, decode_pack_with_bases, pack_index_entries,
	ref_delta_base_ids,
};
use gitana_repository::Repository;

const UPLOAD_PACK_REQUEST: &str = "application/x-git-upload-pack-request";
/// Content type for a `git-receive-pack` request body.
pub const RECEIVE_PACK_REQUEST: &str = "application/x-git-receive-pack-request";

/// The default fetch refspec written for a new `origin` remote (and the fallback when a remote has no
/// configured `fetch` line): mirror every remote branch into `refs/remotes/origin/*`, force-updated.
pub const ORIGIN_FETCH_REFSPEC: &str = "+refs/heads/*:refs/remotes/origin/*";

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

	/// Persist the origin as a standard git remote in the repository's `config`, written
	/// through the `store` capability — so this works over any [`FileStore`] (a local
	/// checkout or the wasm descriptor backend) with no ambient filesystem access.
	pub async fn save(&self, store: &impl FileStore) -> Result<()> {
		let bytes = store.read_path("config").await.context("reading config")?;
		let text = String::from_utf8(bytes).context("config is not UTF-8")?;
		let mut config = GitConfig::parse(&text).context("parsing config")?;
		config.set("remote", Some("origin"), "url", &self.url)?;
		config.set("remote", Some("origin"), "fetch", ORIGIN_FETCH_REFSPEC)?;
		store
			.write_path_replace("config", config.render().as_bytes())
			.await
			.context("writing config")?;
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

/// Fetch the raw `GET /info/refs` advertisement bytes for `service` (`git-upload-pack`
/// or `git-receive-pack`). Hash-agnostic: the caller reads the advertised object-format
/// from the result ([`negotiated_kind`]) before parsing oids under a concrete `H`.
pub async fn fetch_advertisement(
	transport: &impl HttpTransport,
	origin: &Origin,
	service: &str,
) -> Result<Vec<u8>> {
	transport.get(&origin.info_refs(service)).await
}

/// The hash algorithm a remote advertises, from the `object-format` capability in its
/// advertisement. An absent capability (or an explicit `sha1`) is git's default.
pub fn negotiated_kind(body: &[u8]) -> Result<HashKind> {
	match peek_object_format(body).as_deref() {
		Some("sha256") => Ok(HashKind::Sha256),
		None | Some("sha1") => Ok(HashKind::Sha1),
		Some(other) => bail!("remote advertises unsupported object-format: {other}"),
	}
}

/// Require the remote's advertised object format to match the local repository's, since
/// objects of one hash cannot be stored in a repository of the other.
pub fn ensure_same_format(local: HashKind, remote: HashKind) -> Result<()> {
	if local != remote {
		bail!(
			"remote object-format is {}, but the local repository is {}",
			remote.name(),
			local.name()
		);
	}
	Ok(())
}

/// Download the objects reachable from `wants` but not `haves` into `repo`.
///
/// `deepen` requests a shallow history (git's `--depth` / `--shallow-since` / `--shallow-exclude`).
/// The repository's current shallow boundary (`.git/shallow`) is sent so the server knows which
/// history it already truncated, and the boundary the server reports back is persisted — so a plain
/// (non-shallow, empty `deepen`) fetch behaves exactly as before.
///
/// `include_tag` asks the server to append annotated tags reachable from the fetched history (git's
/// `include-tag`) — a shallow fetch/clone sets it so tags pointing into the truncated history still
/// arrive, but `--no-tags` clears it (git omits `include-tag` then). A full fetch wants every ref
/// explicitly, so it passes `false`.
pub async fn fetch_pack<H: HashAlgorithm>(
	transport: &impl HttpTransport,
	origin: &Origin,
	repo: &Repository<impl FileStore, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
	deepen: &Deepen,
	include_tag: bool,
) -> Result<()> {
	if wants.is_empty() {
		return Ok(());
	}
	let shallow = repo.read_shallow().await?;

	// A shallow / deepen fetch negotiates through the `deepen` protocol, not have-batching, so it sends a
	// single final round (`done`) carrying the ref-tip haves and the current boundary.
	if !deepen.is_empty() || !shallow.is_empty() {
		let request = build_upload_pack_request(wants, haves, &shallow, deepen, include_tag, true);
		let response = post_upload_pack(transport, origin, request).await?;
		let response = parse_upload_pack_response::<H>(&response)?;
		return store_response(repo, &shallow, response).await;
	}

	// A plain fetch negotiates: offer local commits (walked back from the ref-tip `haves`) in batches,
	// ending with `done` once the server signals `ready` or the haves run out — so it can cut the pack at
	// the deepest shared commit. A server that ignores `multi_ack_detailed` and sends the pack on the
	// first round is handled too: we take the pack as soon as one arrives.
	let mut remaining = collect_have_commits(repo, haves).await?;
	let mut offered: Vec<ObjectId<H>> = Vec::new();
	let mut ready = false;
	loop {
		let done = ready || remaining.is_empty();
		if !done {
			let batch = remaining.len().min(HAVE_BATCH);
			offered.extend(remaining.drain(..batch));
		}
		let request = build_upload_pack_request(wants, &offered, &[], deepen, include_tag, done);
		let response = post_upload_pack(transport, origin, request).await?;
		let response = parse_upload_pack_response::<H>(&response)?;
		if !response.pack.is_empty() {
			return store_response(repo, &shallow, response).await;
		}
		// A negotiation round carried only acknowledgments. Once we have sent `done`, the server owed us a
		// pack — an empty body is a server-side failure (e.g. `git http-backend` exiting after the headers).
		if done {
			bail!(
				"the remote returned no packfile; the upload-pack request may have failed on the server"
			);
		}
		ready = response.ready;
	}
}

/// The maximum `have`s offered per negotiation round.
const HAVE_BATCH: usize = 16;

/// A cap on the local commits walked to offer as `have`s, bounding a deep-divergence negotiation to a
/// handful of rounds (git similarly stops after a bounded number of unacknowledged haves). Beyond it the
/// client sends `done`; the pack may then be larger than optimal but is still correct.
const HAVE_CAP: usize = 256;

/// POST an upload-pack request and return the raw response body.
async fn post_upload_pack(
	transport: &impl HttpTransport,
	origin: &Origin,
	request: Vec<u8>,
) -> Result<Vec<u8>> {
	transport
		.post(&origin.upload_pack(), UPLOAD_PACK_REQUEST, request)
		.await
}

/// Store a fetched pack response and fold in any shallow-boundary update.
async fn store_response<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	shallow_before: &[ObjectId<H>],
	response: gitana_git_http::UploadPackResponse<H>,
) -> Result<()> {
	// No packfile at all means the server sent nothing — e.g. `git http-backend` exiting after the HTTP
	// headers when a `--shallow-exclude` ref cannot be resolved (a 200 with an empty body). A legitimate
	// up-to-date fetch still carries a valid 0-object packfile (12-byte header + trailer), so an empty
	// body is a failure, not "nothing new" — surface it rather than report a successful empty clone.
	if response.pack.is_empty() {
		bail!("the remote returned no packfile; the upload-pack request may have failed on the server");
	}
	// Skip a valid but empty pack (server had nothing new). The 12-byte header carries the count.
	if response.pack.len() >= 12 && pack_object_count(&response.pack) > 0 {
		store_fetched_pack(repo, response.pack)
			.await
			.context("storing fetched pack")?;
	}
	persist_shallow(repo, shallow_before, &response.shallow, &response.unshallow).await?;
	Ok(())
}

/// Walk local commits to offer as negotiation `have`s: a breadth-first sweep back from the ref-tip
/// `roots` (tags peeled to their commit), newest-ish first, capped at [`HAVE_CAP`] to bound a
/// deep-divergence negotiation. Best-effort — an unreadable or non-commit tip is simply not offered.
async fn collect_have_commits<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	roots: &[ObjectId<H>],
) -> Result<Vec<ObjectId<H>>> {
	use std::collections::{HashSet, VecDeque};

	let mut out: Vec<ObjectId<H>> = Vec::new();
	let mut seen: HashSet<ObjectId<H>> = HashSet::new();
	let mut queue: VecDeque<ObjectId<H>> = roots.iter().copied().collect();
	while let Some(id) = queue.pop_front() {
		if out.len() >= HAVE_CAP {
			break;
		}
		if !seen.insert(id) {
			continue;
		}
		// Best-effort: a ref we cannot read (or a tree/blob tip) is just not offered as a have.
		if let Ok((kind, data)) = repo.objects().read_object(&id).await {
			match kind {
				gitana_object::ObjectKind::Commit => {
					out.push(id);
					queue.extend(gitana_object::parse_commit::<H>(&data)?.parents);
				}
				gitana_object::ObjectKind::Tag => {
					queue.push_back(gitana_object::parse_tag::<H>(&data)?.object);
				}
				_ => {}
			}
		}
	}
	Ok(out)
}

/// Fold the server's shallow-boundary update into `.git/shallow`: the new boundary is the previous
/// boundary plus the commits the server declared `shallow`, minus those it `unshallow`ed (whose
/// parents it just sent). Only rewrites the file when the set actually changes, so a non-shallow fetch
/// never touches it.
async fn persist_shallow<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	previous: &[ObjectId<H>],
	added: &[ObjectId<H>],
	removed: &[ObjectId<H>],
) -> Result<()> {
	if added.is_empty() && removed.is_empty() {
		return Ok(());
	}
	let removed: std::collections::HashSet<ObjectId<H>> = removed.iter().copied().collect();
	let mut boundary: Vec<ObjectId<H>> = Vec::new();
	let mut seen = std::collections::HashSet::new();
	for oid in previous.iter().chain(added).copied() {
		if !removed.contains(&oid) && seen.insert(oid) {
			boundary.push(oid);
		}
	}
	repo.write_shallow(&boundary).await?;
	Ok(())
}

/// Persist a fetched pack. A self-contained pack is stored whole (pack + `.idx`, for
/// random access), including one whose `REF_DELTA` bases all live in the pack. A **thin**
/// pack — one whose deltas reference a base *outside* it, which the server may send
/// because we negotiate `thin-pack` — cannot be stored directly (`write_pack` rejects
/// it); complete it against the bases already in our store and materialise its objects
/// loose, mirroring the receive-pack unpack path.
async fn store_fetched_pack<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	pack: Vec<u8>,
) -> Result<()> {
	// No REF deltas at all → self-contained (the common clone/OFS case); a cheap header
	// scan avoids decoding the whole pack twice.
	if ref_delta_base_ids::<H>(&pack)?.is_empty() {
		repo.objects().write_pack(pack).await?;
		return Ok(());
	}
	// There are REF deltas: their bases may be carried in the pack (still self-contained)
	// or external (thin). Probe by indexing — it resolves every delta, so it succeeds iff
	// self-contained and fails with `UnresolvedDeltaBase` iff a base is genuinely missing.
	match pack_index_entries::<H>(&pack) {
		Ok(_) => repo.objects().write_pack(pack).await?,
		Err(ObjectError::UnresolvedDeltaBase) => complete_thin_pack(repo, &pack).await?,
		Err(other) => return Err(other.into()),
	}
	Ok(())
}

/// Complete a thin pack against the objects already in `repo` and store its objects loose.
async fn complete_thin_pack<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	pack: &[u8],
) -> Result<()> {
	// Supply every referenced base we already have; in-pack bases resolve from the pack,
	// external ones from this map. A base we lack surfaces as a decode error.
	let mut bases = HashMap::new();
	for id in ref_delta_base_ids::<H>(pack)? {
		if let Ok(object) = repo.objects().read_object(&id).await {
			bases.insert(id, object);
		}
	}
	for object in decode_pack_with_bases(pack, &bases)? {
		repo
			.objects()
			.write_object(object.kind, &object.data)
			.await?;
	}
	Ok(())
}

/// The object count from a packfile's v2 header.
fn pack_object_count(pack: &[u8]) -> u32 {
	u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]])
}

/// The unique object ids advertised across all refs (fetch wants / push haves).
pub fn advertised_oids<H: HashAlgorithm>(advertised: &Advertised<H>) -> Vec<ObjectId<H>> {
	let mut oids: Vec<ObjectId<H>> = advertised.refs.iter().map(|(_, oid)| *oid).collect();
	oids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
	oids.dedup();
	oids
}

/// The unique tips of every local ref, sent as fetch `have`s so the server omits
/// objects we already hold.
pub async fn local_haves<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
) -> Result<Vec<ObjectId<H>>> {
	let mut oids: Vec<ObjectId<H>> = repo
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
	use gitana_file_store_memory::MemoryFileStore;

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

	#[tokio::test]
	async fn origin_saves_through_the_file_store() {
		let store = MemoryFileStore::new();
		store
			.write_path_replace(
				"config",
				b"[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha256\n",
			)
			.await
			.unwrap();

		let origin = Origin::parse("https://example.com/acme/app.git").unwrap();
		origin.save(&store).await.unwrap();

		let config = String::from_utf8(store.read_path("config").await.unwrap()).unwrap();
		assert!(config.contains("[remote \"origin\"]"));
		assert!(config.contains("url = https://example.com/acme/app.git"));
		assert!(config.contains("fetch = +refs/heads/*:refs/remotes/origin/*"));
		// The saved config parses back to the same origin (what `Origin::load` reads).
		let parsed = GitConfig::parse(&config).unwrap();
		assert_eq!(
			parsed.get_string("remote", Some("origin"), "url"),
			Some(origin.url.as_str())
		);
	}
}
