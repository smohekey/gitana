//! A [`PackFetcher`] over an SSH connection — git's stateful `multi_ack_detailed` negotiation.

use anyhow::Result;
use gitana_file_store::FileStore;
use gitana_git_http::{Deepen, build_upload_pack_request, parse_upload_pack_response};
use gitana_object::{HashAlgorithm, ObjectId, write_flush, write_pkt};
use gitana_repository::Repository;

use crate::{
	Connection, HAVE_BATCH, PackFetcher, SshConnection, collect_have_commits, store_response,
};

/// A [`PackFetcher`] that negotiates over a single stateful SSH stream. Unlike Smart HTTP's
/// stateless-RPC (re-POST each round), the client sends the wants once and then reads the server's ACK
/// batch after each have-group, sending more haves until the server is `ready` (or they run out), then
/// `done` — all on the one connection.
pub struct SshPackFetcher {
	connection: SshConnection,
}

impl SshPackFetcher {
	/// A fetcher over an already-opened `git-upload-pack` connection (its ref advertisement already read).
	pub fn new(connection: SshConnection) -> Self {
		Self { connection }
	}
}

impl PackFetcher for SshPackFetcher {
	async fn fetch_pack<F: FileStore, H: HashAlgorithm>(
		&mut self,
		repo: &Repository<F, H>,
		wants: &[ObjectId<H>],
		haves: &[ObjectId<H>],
		deepen: &Deepen,
		include_tag: bool,
	) -> Result<()> {
		negotiate(
			&mut self.connection,
			repo,
			wants,
			haves,
			deepen,
			include_tag,
		)
		.await
	}
}

/// Drive git's stateful fetch negotiation over `connection`.
async fn negotiate<F: FileStore, H: HashAlgorithm>(
	connection: &mut SshConnection,
	repo: &Repository<F, H>,
	wants: &[ObjectId<H>],
	haves: &[ObjectId<H>],
	deepen: &Deepen,
	include_tag: bool,
) -> Result<()> {
	// Nothing wanted (an up-to-date fetch): still finalise the session (flush + ssh exit status).
	if wants.is_empty() {
		return connection.finish().await;
	}

	let shallow = repo.read_shallow().await?;
	// A shallow / deepen fetch negotiates through the deepen protocol, not have-batching — a single round
	// carrying the ref-tip haves and the current boundary, exactly as the stateless-HTTP path does.
	if !deepen.is_empty() || !shallow.is_empty() {
		let request = build_upload_pack_request(wants, haves, &shallow, deepen, include_tag, true);
		connection.write(&request).await?;
		return finish_pack(connection, repo, &shallow).await;
	}

	// Plain fetch: `multi_ack_detailed`. Round 0 sends the wants and the first have-group (no `done`);
	// each later round sends another have-group and reads its ACK batch, until the server signals `ready`
	// or the haves run out. Then `done` commits and the server sends the pack.
	let mut remaining = collect_have_commits(repo, haves).await?;
	let first = drain_group(&mut remaining);
	let request = build_upload_pack_request(wants, &first, &[], deepen, include_tag, false);
	connection.write(&request).await?;
	let mut ready = connection.read_ack_batch().await?;
	while !ready && !remaining.is_empty() {
		let group = drain_group(&mut remaining);
		connection.write(&have_group(&group)).await?;
		ready = connection.read_ack_batch().await?;
	}
	connection.write(&done_line()).await?;
	finish_pack(connection, repo, &shallow).await
}

/// Read the pack response after the client's final `done`, store it, and await ssh's exit.
async fn finish_pack<F: FileStore, H: HashAlgorithm>(
	connection: &mut SshConnection,
	repo: &Repository<F, H>,
	shallow_before: &[ObjectId<H>],
) -> Result<()> {
	let response = connection.read_pack().await?;
	let response = parse_upload_pack_response::<H>(&response)?;
	store_response(repo, shallow_before, response).await?;
	connection.finish().await
}

/// Take up to [`HAVE_BATCH`] haves off the front of `remaining`.
fn drain_group<H: HashAlgorithm>(remaining: &mut Vec<ObjectId<H>>) -> Vec<ObjectId<H>> {
	let count = remaining.len().min(HAVE_BATCH);
	remaining.drain(..count).collect()
}

/// A have-group message: `have <oid>` pkt-lines terminated by a flush (no wants — those were sent once).
fn have_group<H: HashAlgorithm>(haves: &[ObjectId<H>]) -> Vec<u8> {
	let mut out = Vec::new();
	for have in haves {
		let _ = write_pkt(&mut out, format!("have {}\n", have.to_hex()).as_bytes());
	}
	write_flush(&mut out);
	out
}

/// The terminating `done` pkt-line.
fn done_line() -> Vec<u8> {
	let mut out = Vec::new();
	let _ = write_pkt(&mut out, b"done\n");
	out
}
