//! The protocol-v2 `ls-refs` command: list refs (optionally filtered by prefix, with
//! symref targets and peeled tags), invoked by `POST /git-upload-pack`.

use gitana_file_store::FileStore;
use gitana_object::{PktLine, parse_pkt, write_flush, write_pkt};
use gitana_repository::Repository;

use crate::GitHttpError;
use crate::refs::collect_refs;

/// Parsed `ls-refs` arguments.
struct LsRefsArgs {
	/// Include `symref-target:<ref>` for symbolic refs (HEAD).
	symrefs: bool,
	/// Include `peeled:<oid>` for annotated tags.
	peel: bool,
	/// Only advertise refs whose name starts with one of these (empty = all).
	ref_prefixes: Vec<String>,
}

/// Handle an `ls-refs` request body, returning the v2 ref-listing response.
pub async fn ls_refs(
	repo: &Repository<impl FileStore>,
	request: &[u8],
) -> Result<Vec<u8>, GitHttpError> {
	let args = parse_ls_refs(request)?;
	let refs = collect_refs(repo, args.peel).await?;

	let mut out = Vec::new();
	for line in &refs {
		if !matches_prefix(&line.name, &args.ref_prefixes) {
			continue;
		}
		let mut rendered = format!("{} {}", line.oid, line.name);
		if args.symrefs
			&& let Some(target) = &line.symref_target
		{
			rendered.push_str(&format!(" symref-target:{target}"));
		}
		if args.peel
			&& let Some(peeled) = line.peeled
		{
			rendered.push_str(&format!(" peeled:{peeled}"));
		}
		rendered.push('\n');
		write_pkt(&mut out, rendered.as_bytes())?;
	}
	write_flush(&mut out);
	Ok(out)
}

/// Parse the `ls-refs` command body: a `command=ls-refs` line, capability lines, a
/// delimiter, then the arguments (`peel`, `symrefs`, `ref-prefix <p>`).
fn parse_ls_refs(request: &[u8]) -> Result<LsRefsArgs, GitHttpError> {
	let mut args = LsRefsArgs {
		symrefs: false,
		peel: false,
		ref_prefixes: Vec::new(),
	};
	let mut saw_command = false;

	let mut cursor = 0;
	while cursor < request.len() {
		let (line, consumed) = parse_pkt(&request[cursor..])?;
		cursor += consumed;
		let PktLine::Data(data) = line else {
			if line == PktLine::Flush {
				break;
			}
			continue;
		};
		let text = std::str::from_utf8(data)
			.map_err(|_| GitHttpError::MalformedRequest("non-utf8 pkt-line".to_owned()))?
			.trim_end_matches('\n');
		match text {
			"command=ls-refs" => saw_command = true,
			"peel" => args.peel = true,
			"symrefs" => args.symrefs = true,
			_ => {
				if let Some(prefix) = text.strip_prefix("ref-prefix ") {
					args.ref_prefixes.push(prefix.to_owned());
				}
			}
		}
	}

	if !saw_command {
		return Err(GitHttpError::MalformedRequest(
			"not an ls-refs command".to_owned(),
		));
	}
	Ok(args)
}

/// Whether `name` passes the prefix filter (empty filter matches everything).
fn matches_prefix(name: &str, prefixes: &[String]) -> bool {
	prefixes.is_empty() || prefixes.iter().any(|prefix| name.starts_with(prefix))
}
