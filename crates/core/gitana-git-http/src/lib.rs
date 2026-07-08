//! Git Smart-HTTP protocol state machines, transport-agnostic.
//!
//! Turns request bytes into response bytes over a [`Repository`](gitana_repository::Repository),
//! with no HTTP knowledge. An embedding HTTP layer owns routes, headers, and body
//! streaming. This crate covers ref advertisement, v2 `ls-refs`, fetch/upload-pack,
//! and receive-pack.

mod advertise;
mod client;
mod deepen;
mod enforce;
mod fetch;
mod ls_refs;
mod negotiate;
mod no_replay_check;
mod nonce_ledger;
mod pack;
mod push_cert;
mod receive_pack;
mod refs;
mod service;
mod shallow;
mod sideband;
mod upload_pack_v0;

pub use advertise::{AGENT, advertise};
pub use client::{
	Advertised, RefUpdate, UploadPackResponse, build_receive_pack_request, build_upload_pack_request,
	parse_advertisement, parse_report_status, parse_upload_pack_response, peek_object_format,
};
pub use deepen::Deepen;
pub use enforce::{TrustContext, TrustVerdict, verify_push, verify_push_with_ledger};
pub use fetch::fetch;
pub use gitana_trust::AuditEvent;
pub use ls_refs::ls_refs;
pub use no_replay_check::NoReplayCheck;
pub use nonce_ledger::NonceLedger;
pub use pack::{build_pack, build_pack_thin};
pub use push_cert::{
	CertCommand, PushCert, build as build_push_cert, make_nonce, peek as peek_push_cert, verify_nonce,
};
pub use receive_pack::{
	ReceiveOptions, ReceiveOutcome, command_ref_names, receive_pack, rejection_report,
};
pub use service::{ProtocolVersion, Service};
pub use upload_pack_v0::upload_pack_v0;

use gitana_object::ObjectError;
use gitana_object_store::ObjectStoreError;
use gitana_repository::RepositoryError;

/// Errors from serving a Smart-HTTP request.
#[derive(Debug, thiserror::Error)]
pub enum GitHttpError {
	/// The requested service is not one this server speaks.
	#[error("unsupported service: {0}")]
	UnsupportedService(String),
	/// A request body was malformed (bad pkt-line, missing command, etc.).
	#[error("malformed request: {0}")]
	MalformedRequest(String),
	/// Reading refs from the repository failed.
	#[error("repository error: {0}")]
	Repository(#[from] RepositoryError),
	/// Reading objects from the object store failed.
	#[error("object store error: {0}")]
	ObjectStore(#[from] ObjectStoreError),
	/// Encoding or decoding wire bytes failed.
	#[error("codec error: {0}")]
	Codec(#[from] ObjectError),
	/// A host push-nonce ledger lookup failed (see [`NonceLedger`]).
	#[error("nonce ledger error: {0}")]
	NonceLedger(String),
}
