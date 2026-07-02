use std::marker::PhantomData;

use gitana_file_store::{FileStore, FileStoreError};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{HeadState, RepositoryError};

/// The maximum symbolic-ref chain depth to follow (git's limit), a guard against a cycle.
const MAX_SYMREF_DEPTH: usize = 5;

/// Reads and updates refs (loose files + symbolic HEAD) over a file store.
///
/// Borrows the repository's file store and id, so it shares the one backend the
/// object store already holds. packed-refs reading and the reflog land in later
/// phases (see docs/hlds/repository-engine.md). Generic over the hash algorithm `H`,
/// which fixes the width of the object ids refs resolve to.
pub struct RefStore<'a, F, H> {
	files: &'a F,
	_hash: PhantomData<H>,
}

impl<'a, F, H> RefStore<'a, F, H>
where
	F: FileStore,
	H: HashAlgorithm,
{
	/// Build a ref store over `files` for `repo`.
	pub fn new(files: &'a F) -> Self {
		Self {
			files,
			_hash: PhantomData,
		}
	}

	/// Read and parse `HEAD`.
	pub async fn read_head(&self) -> Result<HeadState<H>, RepositoryError> {
		match self.files.read_path("HEAD").await {
			Ok(bytes) => HeadState::parse(&bytes),
			Err(FileStoreError::NotFound) => Err(RepositoryError::InvalidRef("no HEAD".to_owned())),
			Err(other) => Err(other.into()),
		}
	}

	/// Resolve a ref to its object id, or `None` if it does not exist. Tries the
	/// loose ref file, then git's `packed-refs` (e.g. after `git pack-refs`).
	pub async fn resolve(&self, name: &str) -> Result<Option<ObjectId<H>>, RepositoryError> {
		match self.files.read_path(name).await {
			Ok(bytes) => Ok(Some(parse_oid(name, &bytes)?)),
			Err(FileStoreError::NotFound) => self.resolve_packed(name).await,
			Err(other) => Err(other.into()),
		}
	}

	/// Look up `name` in git's `packed-refs` file.
	async fn resolve_packed(&self, name: &str) -> Result<Option<ObjectId<H>>, RepositoryError> {
		let bytes = match self.files.read_path("packed-refs").await {
			Ok(bytes) => bytes,
			Err(FileStoreError::NotFound) => return Ok(None),
			Err(other) => return Err(other.into()),
		};
		let text = std::str::from_utf8(&bytes)
			.map_err(|_| RepositoryError::InvalidRef("packed-refs not UTF-8".to_owned()))?;
		for line in text.lines() {
			// Skip the header and `^<peeled>` lines.
			if line.starts_with('#') || line.starts_with('^') || line.is_empty() {
				continue;
			}
			if let Some((oid, refname)) = line.split_once(' ')
				&& refname == name
			{
				return Ok(Some(parse_oid(name, oid.as_bytes())?));
			}
		}
		Ok(None)
	}

	/// List refs under `prefix` (e.g. `refs/heads/`), merging `packed-refs` with
	/// loose ref files (loose wins). Recurses into subdirectories so hierarchical
	/// names (`refs/heads/feature/x`) are included. Symbolic loose refs are skipped.
	/// Returns `(full ref name, oid)` pairs sorted by name.
	pub async fn list(&self, prefix: &str) -> Result<Vec<(String, ObjectId<H>)>, RepositoryError> {
		use std::collections::BTreeMap;
		let mut refs: BTreeMap<String, ObjectId<H>> = BTreeMap::new();

		// packed-refs first; loose files override.
		if let Some(bytes) = self.read_opt("packed-refs").await? {
			let text = std::str::from_utf8(&bytes)
				.map_err(|_| RepositoryError::InvalidRef("packed-refs not UTF-8".to_owned()))?;
			for line in text.lines() {
				if line.starts_with('#') || line.starts_with('^') || line.is_empty() {
					continue;
				}
				if let Some((oid, name)) = line.split_once(' ')
					&& name.starts_with(prefix)
				{
					refs.insert(name.to_owned(), parse_oid(name, oid.as_bytes())?);
				}
			}
		}

		// Loose refs: walk the directory tree under `prefix`.
		let mut stack = vec![prefix.to_owned()];
		while let Some(dir) = stack.pop() {
			for path in self.files.list_prefix(&dir).await? {
				match self.files.read_path(&path).await {
					Ok(bytes) => {
						let text = std::str::from_utf8(&bytes).map(str::trim).unwrap_or("");
						if !text.starts_with("ref:")
							&& let Ok(oid) = ObjectId::from_hex(text)
						{
							refs.insert(path, oid);
						}
					}
					// A read failure here means `path` is a subdirectory; descend.
					Err(_) => stack.push(format!("{path}/")),
				}
			}
		}

		Ok(refs.into_iter().collect())
	}

	/// Read a path, mapping `NotFound` to `None`.
	async fn read_opt(&self, path: &str) -> Result<Option<Vec<u8>>, RepositoryError> {
		match self.files.read_path(path).await {
			Ok(bytes) => Ok(Some(bytes)),
			Err(FileStoreError::NotFound) => Ok(None),
			Err(other) => Err(other.into()),
		}
	}

	/// Resolve `HEAD` to a commit id, following its symbolic target (through a chain of symbolic
	/// refs, as git does). Returns `None` for an unborn branch (symbolic target with no ref file
	/// yet).
	pub async fn resolve_head(&self) -> Result<Option<ObjectId<H>>, RepositoryError> {
		match self.read_head().await? {
			HeadState::Detached(id) => Ok(Some(id)),
			HeadState::Symbolic(target) => self.follow_symref(&target).await,
		}
	}

	/// The object ids that symbolic refs under `prefix` resolve to (following `ref:` chains).
	/// [`Self::list`] returns only direct refs and skips symbolic ones; this recovers those, so a
	/// prune keeps a commit reachable only through a symbolic ref (e.g. `refs/heads/alias` →
	/// `CUSTOM_REF`). A symbolic ref whose chain ends nowhere resolves to nothing and is ignored.
	pub async fn symbolic_ref_targets(
		&self,
		prefix: &str,
	) -> Result<Vec<ObjectId<H>>, RepositoryError> {
		let mut ids = Vec::new();
		let mut stack = vec![prefix.to_owned()];
		while let Some(dir) = stack.pop() {
			for path in self.files.list_prefix(&dir).await? {
				match self.files.read_path(&path).await {
					Ok(bytes) => {
						let text = std::str::from_utf8(&bytes).map(str::trim).unwrap_or("");
						if let Some(target) = text.strip_prefix("ref:")
							&& let Some(id) = self.follow_symref(target.trim()).await?
						{
							ids.push(id);
						}
					}
					// A read failure here means `path` is a subdirectory; descend (as `list` does).
					Err(_) => stack.push(format!("{path}/")),
				}
			}
		}
		Ok(ids)
	}

	/// Resolve `name` to an object id, following a bounded chain of symbolic (`ref:`) refs and
	/// consulting `packed-refs` for a target with no loose file. `None` if the chain ends at a
	/// missing ref or exceeds the depth bound (a cycle).
	async fn follow_symref(&self, name: &str) -> Result<Option<ObjectId<H>>, RepositoryError> {
		let mut name = name.to_owned();
		for _ in 0..MAX_SYMREF_DEPTH {
			match self.files.read_path(&name).await {
				Ok(bytes) => {
					let text = std::str::from_utf8(&bytes).map(str::trim).unwrap_or("");
					match text.strip_prefix("ref:") {
						Some(target) => name = target.trim().to_owned(),
						None => return Ok(Some(parse_oid(&name, &bytes)?)),
					}
				}
				Err(FileStoreError::NotFound) => return self.resolve_packed(&name).await,
				Err(other) => return Err(other.into()),
			}
		}
		Ok(None)
	}

	/// Compare-and-set a ref. `expected == None` requires the ref to be absent;
	/// otherwise the current value must equal `expected`.
	pub async fn update_ref(
		&self,
		name: &str,
		new: ObjectId<H>,
		expected: Option<ObjectId<H>>,
	) -> Result<(), RepositoryError> {
		let bytes = format!("{new}\n");
		match self.files.read_path_versioned(name).await {
			Ok((current_bytes, version)) => {
				let current = parse_oid(name, &current_bytes)?;
				if expected != Some(current) {
					return Err(RepositoryError::RefMoved {
						name: name.to_owned(),
					});
				}
				self
					.files
					.write_path_cas(name, bytes.as_bytes(), Some(&version))
					.await
					.map_err(map_cas(name))?;
			}
			Err(FileStoreError::NotFound) => {
				if expected.is_some() {
					return Err(RepositoryError::RefMoved {
						name: name.to_owned(),
					});
				}
				self
					.files
					.write_path_cas(name, bytes.as_bytes(), None)
					.await
					.map_err(map_cas(name))?;
			}
			Err(other) => return Err(other.into()),
		}
		Ok(())
	}

	/// Delete a ref, requiring its current resolved value to equal `expected` (CAS).
	///
	/// Removes the loose ref file (if any) and drops the ref from `packed-refs` (if
	/// present), so the ref no longer resolves by either path. Errors with
	/// [`RepositoryError::RefMoved`] if the current value differs from `expected`.
	pub async fn delete_ref(
		&self,
		name: &str,
		expected: Option<ObjectId<H>>,
	) -> Result<(), RepositoryError> {
		let current = self.resolve(name).await?;
		if current != expected {
			return Err(RepositoryError::RefMoved {
				name: name.to_owned(),
			});
		}
		if current.is_none() {
			return Err(RepositoryError::InvalidRef(format!("{name}: no such ref")));
		}
		// Delete the loose ref file under a version check (no-op if packed-only).
		match self.files.read_path_versioned(name).await {
			Ok((_, version)) => {
				self
					.files
					.delete_path(name, Some(&version))
					.await
					.map_err(map_cas(name))?;
			}
			Err(FileStoreError::NotFound) => {}
			Err(other) => return Err(other.into()),
		}
		// Drop the ref (and its peeled line) from packed-refs if present.
		self.remove_from_packed(name).await
	}

	/// Rewrite `packed-refs` without `name` (and its `^<peeled>` continuation line).
	/// A no-op if there is no packed-refs file or the ref is not packed.
	async fn remove_from_packed(&self, name: &str) -> Result<(), RepositoryError> {
		let Some(bytes) = self.read_opt("packed-refs").await? else {
			return Ok(());
		};
		let text = std::str::from_utf8(&bytes)
			.map_err(|_| RepositoryError::InvalidRef("packed-refs not UTF-8".to_owned()))?;
		let mut out = String::with_capacity(text.len());
		let mut changed = false;
		let mut drop_peeled = false;
		for line in text.lines() {
			// A `^<peeled>` line belongs to the entry above it; drop it with that entry.
			if drop_peeled && line.starts_with('^') {
				drop_peeled = false;
				continue;
			}
			drop_peeled = false;
			if !line.starts_with('#')
				&& !line.starts_with('^')
				&& let Some((_, refname)) = line.split_once(' ')
				&& refname == name
			{
				changed = true;
				drop_peeled = true;
				continue;
			}
			out.push_str(line);
			out.push('\n');
		}
		if changed {
			self.force_write("packed-refs", out.as_bytes()).await?;
		}
		Ok(())
	}

	/// Point `HEAD` at a ref name (`ref: <target>`), overwriting any current HEAD.
	pub async fn set_head_symbolic(&self, target: &str) -> Result<(), RepositoryError> {
		self.set_symbolic("HEAD", target).await
	}

	/// Point the symbolic ref `name` (e.g. `HEAD`) at `target`.
	pub async fn set_symbolic(&self, name: &str, target: &str) -> Result<(), RepositoryError> {
		let bytes = HeadState::<H>::Symbolic(target.to_owned()).render();
		self.force_write(name, bytes.as_bytes()).await
	}

	/// The target of a symbolic ref `name`, or `None` if it is absent or not symbolic.
	pub async fn read_symbolic(&self, name: &str) -> Result<Option<String>, RepositoryError> {
		match self.files.read_path(name).await {
			Ok(bytes) => {
				let text = std::str::from_utf8(&bytes)
					.map_err(|_| RepositoryError::InvalidRef(name.to_owned()))?
					.trim();
				Ok(text.strip_prefix("ref: ").map(|t| t.trim().to_owned()))
			}
			Err(FileStoreError::NotFound) => Ok(None),
			Err(other) => Err(other.into()),
		}
	}

	/// Append a reflog entry for `refname` (e.g. `HEAD`, `refs/heads/main`).
	///
	/// The file store has no append, so this is read-modify-write under the caller's
	/// ref lock: `<old> <new> <committer>\t<message>\n` to `logs/<refname>`.
	pub async fn append_reflog(
		&self,
		refname: &str,
		old: Option<ObjectId<H>>,
		new: ObjectId<H>,
		committer: &str,
		message: &str,
	) -> Result<(), RepositoryError> {
		let old = old.map_or_else(|| "0".repeat(H::RAW_LEN * 2), |id| id.to_hex());
		let line = format!("{old} {new} {committer}\t{message}\n");

		let path = format!("logs/{refname}");
		let mut content = match self.files.read_path(&path).await {
			Ok(bytes) => bytes,
			Err(FileStoreError::NotFound) => Vec::new(),
			Err(other) => return Err(other.into()),
		};
		content.extend_from_slice(line.as_bytes());
		self.force_write(&path, &content).await
	}

	/// Every object id referenced by any reflog entry under `logs/`: the `<old>` and `<new>`
	/// id of each line (skipping the all-zero null id of a creation/deletion entry). A prune
	/// keeps these so a commit a reflog can still reach (e.g. before a `reset`) is never deleted.
	pub async fn reflog_object_ids(&self) -> Result<Vec<ObjectId<H>>, RepositoryError> {
		let mut ids = Vec::new();
		let mut stack = vec!["logs/".to_owned()];
		while let Some(dir) = stack.pop() {
			for path in self.files.list_prefix(&dir).await? {
				match self.files.read_path(&path).await {
					Ok(bytes) => {
						// Each line is `<old> <new> <committer>\t<message>`; only the first two
						// whitespace-delimited fields are ids. Parse on raw bytes — the committer and
						// message may hold arbitrary, non-UTF-8 bytes (e.g. a `-m` with binary), so we
						// must not require the whole reflog to be UTF-8.
						for line in bytes.split(|&b| b == b'\n') {
							for token in line
								.split(|b: &u8| b.is_ascii_whitespace())
								.filter(|field| !field.is_empty())
								.take(2)
							{
								if token.iter().all(|&b| b == b'0') {
									continue;
								}
								if let Ok(text) = std::str::from_utf8(token)
									&& let Ok(id) = ObjectId::<H>::from_hex(text)
								{
									ids.push(id);
								}
							}
						}
					}
					// A read failure here means `path` is a subdirectory; descend (as `list` does).
					Err(_) => stack.push(format!("{path}/")),
				}
			}
		}
		Ok(ids)
	}

	/// Unconditional last-writer-wins write, retrying on a concurrent change.
	async fn force_write(&self, path: &str, bytes: &[u8]) -> Result<(), RepositoryError> {
		loop {
			let expected = match self.files.read_path_versioned(path).await {
				Ok((_, version)) => Some(version),
				Err(FileStoreError::NotFound) => None,
				Err(other) => return Err(other.into()),
			};
			match self
				.files
				.write_path_cas(path, bytes, expected.as_ref())
				.await
			{
				Ok(_) => return Ok(()),
				Err(FileStoreError::VersionMismatch) => continue,
				Err(other) => return Err(other.into()),
			}
		}
	}
}

fn parse_oid<H: HashAlgorithm>(name: &str, bytes: &[u8]) -> Result<ObjectId<H>, RepositoryError> {
	let text = std::str::from_utf8(bytes)
		.map_err(|_| RepositoryError::InvalidRef(name.to_owned()))?
		.trim();
	ObjectId::from_hex(text).map_err(|_| RepositoryError::InvalidRef(format!("{name}: {text}")))
}

fn map_cas(name: &str) -> impl Fn(FileStoreError) -> RepositoryError + '_ {
	move |error| match error {
		FileStoreError::VersionMismatch => RepositoryError::RefMoved {
			name: name.to_owned(),
		},
		other => other.into(),
	}
}

#[cfg(test)]
mod tests {
	use gitana_file_store::FileStore;
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::{ObjectId, ObjectKind, Sha256};

	use super::RefStore;

	#[tokio::test]
	async fn reflog_object_ids_collects_ids_despite_a_non_utf8_message() {
		let files = MemoryFileStore::new();
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old tip");
		let new = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"new tip");

		// A reflog line whose message carries a raw non-UTF-8 byte (git allows arbitrary bytes
		// there): the object ids are still ASCII hex and must be read regardless.
		let mut line =
			format!("{} {} C <c@e> 0 +0000\treset: ", old.to_hex(), new.to_hex()).into_bytes();
		line.push(0xff);
		line.push(b'\n');
		// A creation line whose all-zero `<old>` must be skipped.
		line.extend_from_slice(
			format!(
				"{} {} C <c@e> 0 +0000\tcommit\n",
				"0".repeat(64),
				new.to_hex()
			)
			.as_bytes(),
		);
		files
			.write_path_if_absent("logs/HEAD", &line)
			.await
			.unwrap();

		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let ids = store.reflog_object_ids().await.expect("read reflog ids");
		assert!(ids.contains(&old), "old id read");
		assert!(ids.contains(&new), "new id read");
		let zero = ObjectId::<Sha256>::from_hex(&"0".repeat(64)).unwrap();
		assert!(!ids.contains(&zero), "the null id is skipped");
	}

	#[tokio::test]
	async fn symbolic_ref_targets_follows_ref_chains() {
		let files = MemoryFileStore::new();
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"symref target");
		let direct = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"direct");
		// `refs/heads/alias` → `CUSTOM_REF` → an object id (a chain resolving outside refs/).
		files
			.write_path_if_absent("CUSTOM_REF", format!("{}\n", target.to_hex()).as_bytes())
			.await
			.unwrap();
		files
			.write_path_if_absent("refs/heads/alias", b"ref: CUSTOM_REF\n")
			.await
			.unwrap();
		// A direct ref is left to `list`, not returned here.
		files
			.write_path_if_absent(
				"refs/heads/main",
				format!("{}\n", direct.to_hex()).as_bytes(),
			)
			.await
			.unwrap();

		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let ids = store
			.symbolic_ref_targets("refs/")
			.await
			.expect("resolve symbolic refs");
		assert!(ids.contains(&target), "symbolic ref target resolved");
		assert!(!ids.contains(&direct), "direct refs are left to list()");
	}

	#[tokio::test]
	async fn resolve_head_follows_a_symref_chain() {
		let files = MemoryFileStore::new();
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");
		// HEAD → refs/heads/alias → refs/heads/main → an object id.
		files
			.write_path_if_absent("HEAD", b"ref: refs/heads/alias\n")
			.await
			.unwrap();
		files
			.write_path_if_absent("refs/heads/alias", b"ref: refs/heads/main\n")
			.await
			.unwrap();
		files
			.write_path_if_absent("refs/heads/main", format!("{}\n", tip.to_hex()).as_bytes())
			.await
			.unwrap();

		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		assert_eq!(store.resolve_head().await.expect("resolve head"), Some(tip));
	}
}
