//! The `GET /info/refs` advertisement: a protocol-v2 capability list or a v0 ref
//! advertisement, both prefixed by the smart-http service banner.

use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, write_flush, write_pkt};
use gitana_repository::Repository;

use crate::refs::collect_refs;
use crate::{GitHttpError, ProtocolVersion, Service};

/// The agent string gitana reports in capability advertisements.
pub const AGENT: &str = concat!("gitana/", env!("CARGO_PKG_VERSION"));

/// Build the `GET /info/refs?service=…` response body for `service` at `version`.
///
/// The body opens with the smart-http banner (`# service=<name>` + flush), then
/// either the v2 capability advertisement or the v0 ref advertisement. A
/// `push_cert_nonce`, when present on a receive-pack advertisement, is advertised as the
/// `push-cert=<nonce>` capability so clients can sign their push (`git push --signed`).
pub async fn advertise<H: HashAlgorithm>(
	repo: &Repository<impl FileStore, H>,
	service: Service,
	version: ProtocolVersion,
	push_cert_nonce: Option<&str>,
) -> Result<Vec<u8>, GitHttpError> {
	let mut out = Vec::new();
	write_pkt(
		&mut out,
		format!("# service={}\n", service.as_str()).as_bytes(),
	)?;
	write_flush(&mut out);
	match version {
		ProtocolVersion::V2 => write_v2_capabilities::<H>(&mut out)?,
		ProtocolVersion::V0 => write_v0_refs(&mut out, repo, service, push_cert_nonce).await?,
	}
	Ok(out)
}

/// The protocol-v2 capability advertisement: `ls-refs` for ref discovery and `fetch`
/// for object transfer (refs and objects are requested via follow-up commands).
fn write_v2_capabilities<H: HashAlgorithm>(out: &mut Vec<u8>) -> Result<(), GitHttpError> {
	write_pkt(out, b"version 2\n")?;
	write_pkt(out, format!("agent={AGENT}\n").as_bytes())?;
	write_pkt(out, b"ls-refs=unborn\n")?;
	write_pkt(out, b"fetch=ofs-delta shallow\n")?;
	write_pkt(out, format!("object-format={}\n", H::NAME).as_bytes())?;
	write_flush(out);
	Ok(())
}

/// The protocol-v0 ref advertisement: each ref on its own pkt-line, the capabilities
/// trailing the first line after a NUL. An empty repo emits the `capabilities^{}`
/// placeholder so the capability list still reaches the client.
async fn write_v0_refs<H: HashAlgorithm>(
	out: &mut Vec<u8>,
	repo: &Repository<impl FileStore, H>,
	service: Service,
	push_cert_nonce: Option<&str>,
) -> Result<(), GitHttpError> {
	let refs = collect_refs(repo, true).await?;
	let mut caps = base_capabilities::<H>(service);
	if let Some(nonce) = push_cert_nonce {
		caps = format!("{caps} push-cert={nonce}");
	}
	if let Some(target) = refs
		.iter()
		.find(|r| r.name == "HEAD")
		.and_then(|head| head.symref_target.as_deref())
	{
		caps = format!("{caps} symref=HEAD:{target}");
	}

	if refs.is_empty() {
		let zero = "0".repeat(H::RAW_LEN * 2);
		write_pkt(
			out,
			format!("{zero} capabilities^{{}}\0{caps}\n").as_bytes(),
		)?;
	} else {
		for (index, line) in refs.iter().enumerate() {
			if index == 0 {
				write_pkt(
					out,
					format!("{} {}\0{caps}\n", line.oid, line.name).as_bytes(),
				)?;
			} else {
				write_pkt(out, format!("{} {}\n", line.oid, line.name).as_bytes())?;
			}
			if let Some(peeled) = line.peeled {
				write_pkt(out, format!("{peeled} {}^{{}}\n", line.name).as_bytes())?;
			}
		}
	}
	write_flush(out);
	Ok(())
}

/// The v0 capability list for a service, with the codec features the encoder supports
/// and the `object-format` for the hash algorithm `H`.
fn base_capabilities<H: HashAlgorithm>(service: Service) -> String {
	let object_format = H::NAME;
	match service {
		Service::UploadPack => {
			format!(
				"multi_ack_detailed side-band-64k thin-pack ofs-delta shallow deepen-since deepen-not \
				 deepen-relative include-tag object-format={object_format} agent={AGENT}"
			)
		}
		Service::ReceivePack => {
			format!("report-status delete-refs ofs-delta object-format={object_format} agent={AGENT}")
		}
	}
}
