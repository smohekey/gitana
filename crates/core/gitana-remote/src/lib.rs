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

mod auth_transport;
mod credential;
mod credential_provider;
mod credential_request;
mod http_client;
mod http_transport;
mod push_refspec;
mod refspec;
#[cfg(feature = "reqwest-transport")]
mod reqwest_transport;
mod unauthenticated;

pub use auth_transport::AuthTransport;
pub use credential::Credential;
pub use credential_provider::CredentialProvider;
pub use credential_request::CredentialRequest;
pub use http_client::{HttpClient, HttpResponse};
pub use http_transport::HttpTransport;
pub use push_refspec::PushRefspec;
pub use refspec::Refspec;
#[cfg(feature = "reqwest-transport")]
pub use reqwest_transport::ReqwestTransport;
pub use unauthenticated::Unauthenticated;

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

/// Strip any `user[:pass]@` userinfo from `url`, keeping the rest **verbatim** — git's
/// `transport_anonymize_url`, used where a URL is recorded or displayed (e.g. a clone reflog) so a
/// credential in the URL is never persisted, while trailing slashes and the exact path are preserved.
/// A URL without a scheme or userinfo is returned unchanged.
pub fn anonymize_url(url: &str) -> String {
	let Some((scheme, rest)) = url.split_once("://") else {
		return url.to_owned();
	};
	// The authority runs to the first `/`, `?`, or `#`; keep the tail exactly as given.
	let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
	let (authority, tail) = rest.split_at(authority_end);
	match authority.rsplit_once('@') {
		Some((_userinfo, host)) => format!("{scheme}://{host}{tail}"),
		None => url.to_owned(),
	}
}

/// Percent-decode `s` (`%XX` → byte), decoding the resulting bytes as UTF-8 (lossily, since a
/// credential is otherwise opaque). A lone `%` or a `%` not followed by two hex digits is kept
/// literally, matching lenient URL decoders.
fn percent_decode(s: &str) -> String {
	let bytes = s.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%'
			&& i + 2 < bytes.len()
			&& let (Some(hi), Some(lo)) = (
				(bytes[i + 1] as char).to_digit(16),
				(bytes[i + 2] as char).to_digit(16),
			) {
			out.push((hi * 16 + lo) as u8);
			i += 3;
		} else {
			out.push(bytes[i]);
			i += 1;
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode `s` for a URL userinfo field, escaping every byte outside the RFC 3986 unreserved
/// set (`A-Za-z0-9-._~`). Conservative but always safe — it guarantees the persisted username
/// round-trips through `percent_decode`, so a `user@name` becomes `user%40name`. Public so the CLI can
/// build the same credential-prompt URL git does (a username shown re-encoded, e.g. `a%40b`).
pub fn percent_encode_userinfo(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for &byte in s.as_bytes() {
		if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
			out.push(byte as char);
		} else {
			out.push('%');
			out.push_str(&format!("{byte:02X}"));
		}
	}
	out
}

/// The configured origin remote. This is the base Smart HTTP URL, e.g.
/// `https://example.com/acme/project.git`, with any `user[:pass]@` userinfo split off (see
/// [`parse`](Origin::parse)) so the URL sent to the transport carries no embedded credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
	/// The repository base URL, **without** userinfo — safe to send to the transport and to build
	/// endpoints from.
	pub url: String,
	/// A username taken from the URL's `user[:pass]@` userinfo, if any — git's highest-priority
	/// credential hint (and reconstructed into the persisted `remote.origin.url` by [`save`](Origin::save)).
	pub username: Option<String>,
	/// A password taken from the URL's userinfo, if any. Never persisted to config (git stores the
	/// username but not the password); used only for the current operation.
	pub password: Option<String>,
}

impl Origin {
	/// Validate and normalise a Smart HTTP remote URL, splitting off any `user[:pass]@` userinfo. The
	/// userinfo delimiter is the **last** `@` in the authority (so a password may contain an unescaped
	/// `@`), and the username/password split is the first `:` in the userinfo. Each field is
	/// **percent-decoded** (git decodes URL credentials, so `alice%40host`/`p%3Ass` become
	/// `alice@host`/`p:ass`). Field *presence* is preserved: an explicitly empty field is `Some("")`,
	/// not `None` — git treats `https://alice:@host` as the present empty password `alice:` and
	/// `https://:token@host` as username-less. The stored [`url`](Self::url) is the credential-free
	/// remainder.
	pub fn parse(url: &str) -> Result<Self> {
		let trimmed = url.trim_end_matches('/');
		if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
			bail!("only http(s) Smart HTTP remotes are supported");
		}
		// Safe: the scheme check above guarantees a `://`.
		let (scheme, rest) = trimmed.split_once("://").expect("scheme has ://");
		// The authority runs to the first `/`, `?`, or `#`; the rest is the path/query to keep verbatim.
		let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
		let (authority, tail) = rest.split_at(authority_end);
		let (userinfo, host) = match authority.rsplit_once('@') {
			Some((userinfo, host)) => (Some(userinfo), host),
			None => (None, authority),
		};
		// A present userinfo yields a present (possibly empty) username; a `:` additionally introduces a
		// present (possibly empty) password. Absent userinfo → both `None`.
		let (username, password) = match userinfo {
			Some(userinfo) => match userinfo.split_once(':') {
				Some((user, pass)) => (Some(percent_decode(user)), Some(percent_decode(pass))),
				None => (Some(percent_decode(userinfo)), None),
			},
			None => (None, None),
		};
		Ok(Self {
			url: format!("{scheme}://{host}{tail}"),
			username,
			password,
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
		config.set("remote", Some("origin"), "url", &self.persisted_url())?;
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

	/// The URL to persist in `remote.origin.url`: the clean URL with the userinfo **username**
	/// re-embedded (matching git, which keeps the username but never writes the password), so a later
	/// bare `gta fetch`/`push` still knows which user to authenticate as. The username is
	/// percent-encoded so a value with reserved characters (`@`, `:`) round-trips back through
	/// [`parse`](Self::parse).
	fn persisted_url(&self) -> String {
		match &self.username {
			Some(username) => match self.url.split_once("://") {
				Some((scheme, rest)) => format!("{scheme}://{}@{rest}", percent_encode_userinfo(username)),
				None => self.url.clone(),
			},
			None => self.url.clone(),
		}
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

	#[test]
	fn parse_splits_userinfo_off_the_url() {
		let origin = Origin::parse("https://alice:s3cr3t@example.com/acme/app.git").unwrap();
		assert_eq!(origin.url, "https://example.com/acme/app.git");
		assert_eq!(origin.username.as_deref(), Some("alice"));
		assert_eq!(origin.password.as_deref(), Some("s3cr3t"));
	}

	#[test]
	fn parse_keeps_a_bare_username_and_no_userinfo() {
		let user_only = Origin::parse("https://alice@example.com/app").unwrap();
		assert_eq!(user_only.url, "https://example.com/app");
		assert_eq!(user_only.username.as_deref(), Some("alice"));
		assert_eq!(user_only.password, None);

		let none = Origin::parse("https://example.com/app").unwrap();
		assert_eq!(none.username, None);
		assert_eq!(none.password, None);
	}

	#[test]
	fn anonymize_url_strips_userinfo_but_keeps_the_url_verbatim() {
		// Userinfo removed; scheme, host, path, and trailing slash kept exactly.
		assert_eq!(
			anonymize_url("https://alice:s3cr3t@example.com/acme/app.git/"),
			"https://example.com/acme/app.git/"
		);
		// No userinfo → unchanged (including the trailing slash git preserves in the reflog).
		assert_eq!(
			anonymize_url("http://example.com/repo.git/"),
			"http://example.com/repo.git/"
		);
	}

	#[test]
	fn parse_preserves_explicitly_empty_userinfo_fields() {
		// `alice:@host` → a present empty password (git would send `alice:`).
		let empty_pass = Origin::parse("https://alice:@example.com/app").unwrap();
		assert_eq!(empty_pass.username.as_deref(), Some("alice"));
		assert_eq!(empty_pass.password.as_deref(), Some(""));
		// `:token@host` → a present empty username.
		let empty_user = Origin::parse("https://:token@example.com/app").unwrap();
		assert_eq!(empty_user.username.as_deref(), Some(""));
		assert_eq!(empty_user.password.as_deref(), Some("token"));
	}

	#[test]
	fn parse_percent_decodes_userinfo() {
		// `alice%40host` → `alice@host`, `pa%3Ass` → `pa:ss` (git decodes URL credentials).
		let origin = Origin::parse("https://alice%40host:pa%3Ass@example.com/app").unwrap();
		assert_eq!(origin.username.as_deref(), Some("alice@host"));
		assert_eq!(origin.password.as_deref(), Some("pa:ss"));
		assert_eq!(origin.url, "https://example.com/app");
	}

	#[test]
	fn persisted_url_percent_encodes_the_username_round_trip() {
		let origin = Origin::parse("https://alice%40host:pw@example.com/app").unwrap();
		// The persisted url re-encodes the `@` so it parses back to the same username.
		assert_eq!(
			origin.persisted_url(),
			"https://alice%40host@example.com/app"
		);
		let reparsed = Origin::parse(&origin.persisted_url()).unwrap();
		assert_eq!(reparsed.username.as_deref(), Some("alice@host"));
	}

	#[test]
	fn parse_takes_the_last_at_and_first_colon() {
		// A password may contain an unescaped `@`; the delimiter is the last `@` in the authority, and
		// the user/pass split is the first `:`.
		let origin = Origin::parse("https://user:p@ss@example.com/app").unwrap();
		assert_eq!(origin.url, "https://example.com/app");
		assert_eq!(origin.username.as_deref(), Some("user"));
		assert_eq!(origin.password.as_deref(), Some("p@ss"));
	}

	#[test]
	fn save_re_embeds_the_username_but_not_the_password() {
		let origin = Origin::parse("https://alice:s3cr3t@example.com/acme/app.git").unwrap();
		assert_eq!(
			origin.persisted_url(),
			"https://alice@example.com/acme/app.git"
		);
	}
}
