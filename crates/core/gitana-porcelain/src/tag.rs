//! `tag` — create an annotated (optionally SSH-signed) tag object.
//!
//! Unlike a commit's `gpgsig` header, a tag's signature is *appended* after the message (git's
//! `parse_signature`), so signing seals the message-terminated payload and stores the armor with its
//! trailing newline — [`gitana_object::encode_tag`] emits it verbatim. The signed bytes are exactly
//! what git signs, so the object verifies through stock `git tag -v` and the `gitana-trust` core
//! (`verify_tag`) alike.

use anyhow::Result;
use gitana_file_store::FileStore;
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, Tag, encode_tag};
use gitana_repository::Repository;

use crate::Signer;

/// Create an annotated (unsigned) tag `name` pointing at `object`, tagged by `tagger`, and write it —
/// returning the tag object's id. Does not move any ref; the caller updates `refs/tags/<name>`.
pub async fn tag<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	object: ObjectId<H>,
	name: &str,
	tagger: &str,
	message: &str,
) -> Result<ObjectId<H>> {
	let tag = build_tag(repo, object, name, tagger, message).await?;
	write_tag(repo, &tag).await
}

/// Like [`tag`], but sign the tag: the SSHSIG armor covers the exact bytes git signs (the tag without
/// its appended signature block), so stock `git tag -v` and `gitana_trust::verify_tag` both verify it.
pub async fn tag_signed<F: FileStore, H: HashAlgorithm, S: Signer>(
	repo: &Repository<F, H>,
	object: ObjectId<H>,
	name: &str,
	tagger: &str,
	message: &str,
	signer: &S,
) -> Result<ObjectId<H>> {
	let mut tag = build_tag(repo, object, name, tagger, message).await?;
	// Sign the unsigned encoding (what git signs); git appends the armor after the message, so store
	// it with a trailing newline — `encode_tag` emits `signature` verbatim after the message.
	let armor = signer.sign(&encode_tag(&tag)).await?;
	tag.signature = Some(format!("{armor}\n"));
	write_tag(repo, &tag).await
}

/// Build the unsigned annotated-tag object: look up the tagged object's kind (for the `type` line) and
/// normalise the message to end with a newline, as git records it.
async fn build_tag<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	object: ObjectId<H>,
	name: &str,
	tagger: &str,
	message: &str,
) -> Result<Tag<H>> {
	let (kind, _) = repo.objects().read_object(&object).await?;
	let message = if message.ends_with('\n') {
		message.to_owned()
	} else {
		format!("{message}\n")
	};
	Ok(Tag {
		object,
		kind,
		name: name.to_owned(),
		tagger: Some(tagger.to_owned()),
		signature: None,
		message,
	})
}

/// Encode and write the tag object, returning its id.
async fn write_tag<F: FileStore, H: HashAlgorithm>(
	repo: &Repository<F, H>,
	tag: &Tag<H>,
) -> Result<ObjectId<H>> {
	Ok(
		repo
			.objects()
			.write_object(ObjectKind::Tag, &encode_tag(tag))
			.await?,
	)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use gitana_object::{Sha256, parse_tag};
	use gitana_trust::{TrustedKey, verify_tag};

	use super::*;
	use crate::test_support::{TestIdentity, TestSigner, commit_file, fixture};

	const TAGGER: &str = "A U Thor <a@example.com> 0 +0000";

	#[tokio::test]
	async fn signs_a_tag_verifiable_by_the_trust_core() {
		let (dir, wt) = fixture().await;
		let commit = commit_file(
			dir.path(),
			&wt,
			"f.txt",
			b"hello\n",
			&TestIdentity::default(),
		)
		.await;
		let repo = wt.repository();

		let signer = TestSigner::new(2);
		let public_line = signer.public_line();
		let id = tag_signed(repo, commit, "v1", TAGGER, "release", &signer)
			.await
			.unwrap();

		// The raw object carries a signature the real trust core accepts under the signing key — so a
		// `gta tag -s` object verifies the same way `git tag -v` and receive-pack do.
		let (kind, raw) = repo.objects().read_object(&id).await.unwrap();
		assert_eq!(kind, ObjectKind::Tag);
		let key = TrustedKey::from_openssh(&public_line).unwrap();
		let signer_id = verify_tag(&raw, std::slice::from_ref(&key)).unwrap();
		assert_eq!(signer_id, key.id());

		// The tag points at the commit, names the object's kind, and preserves the message.
		let parsed = parse_tag::<Sha256>(&raw).unwrap();
		assert_eq!(parsed.object, commit);
		assert_eq!(parsed.kind, ObjectKind::Commit);
		assert_eq!(parsed.name, "v1");
		assert_eq!(parsed.message, "release\n");
		assert_eq!(parsed.tagger.as_deref(), Some(TAGGER));
	}

	#[tokio::test]
	async fn builds_an_unsigned_annotated_tag() {
		let (dir, wt) = fixture().await;
		let commit = commit_file(
			dir.path(),
			&wt,
			"f.txt",
			b"hello\n",
			&TestIdentity::default(),
		)
		.await;
		let repo = wt.repository();

		let id = tag(repo, commit, "v1", TAGGER, "release").await.unwrap();

		let (kind, raw) = repo.objects().read_object(&id).await.unwrap();
		assert_eq!(kind, ObjectKind::Tag);
		let parsed = parse_tag::<Sha256>(&raw).unwrap();
		assert_eq!(parsed.signature, None);
		assert_eq!(parsed.object, commit);
		assert_eq!(parsed.message, "release\n");
	}
}
