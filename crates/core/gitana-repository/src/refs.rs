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
	/// otherwise the current value must equal `expected`. A ref present only in
	/// `packed-refs` counts as its packed value — updating it writes the loose
	/// file, which shadows the packed entry from then on (as git does).
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
				// No loose file: the ref's current value, if any, is its packed one.
				if expected != self.resolve_packed(name).await? {
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

	/// Delete every ref under `prefix` — loose (direct *or* symbolic), its reflog, and any packed
	/// entry alike. Used to drop a remote's whole `refs/remotes/<name>/` tree, which
	/// [`Self::delete_ref`] cannot: it resolves and value-checks a single, non-symbolic ref.
	pub async fn remove_prefix(&self, prefix: &str) -> Result<(), RepositoryError> {
		// Loose ref files (a symbolic `ref:` file has no oid, so `list` skips it — delete files
		// directly, so `origin/HEAD` goes too), then their reflogs under `logs/`.
		self.delete_files_under(prefix).await?;
		self.delete_files_under(&format!("logs/{prefix}")).await?;
		// Packed: drop every entry whose name is under the prefix.
		self
			.remove_packed_matching(|name| name.starts_with(prefix))
			.await
	}

	/// Move every ref under `old` to `new` — loose (direct *or* symbolic, rewriting a symbolic target
	/// that points back under `old`), its reflog, and any packed entry alike. Used to rename a
	/// remote's whole `refs/remotes/<old>/` tree to `refs/remotes/<new>/`.
	pub async fn rename_prefix(&self, old: &str, new: &str) -> Result<(), RepositoryError> {
		// Loose ref files, rewriting a symbolic `ref:` target that itself points under `old`. Keep the
		// paths we write — they are the destination's *authoritative* loose refs, so the stale-shadow
		// sweep below must not delete them.
		let loose_targets = self
			.move_files_under(old, new, |bytes| {
				if let Ok(text) = std::str::from_utf8(bytes)
					&& let Some(target) = text.trim().strip_prefix("ref:")
					&& let Some(rest) = target.trim().strip_prefix(old)
				{
					return format!("ref: {new}{rest}\n").into_bytes();
				}
				bytes.to_vec()
			})
			.await?;
		// Reflogs, moved verbatim (a message may carry non-UTF-8 bytes).
		self
			.move_files_under(
				&format!("logs/{old}"),
				&format!("logs/{new}"),
				<[u8]>::to_vec,
			)
			.await?;
		// Packed entries: rewrite each `<old>…` ref name to `<new>…`, returning the renamed destinations.
		let renamed_dests = self.rename_packed_prefix(old, new).await?;
		// A stale *loose* ref already sitting at a renamed packed ref's destination would shadow it,
		// leaving the tracking branch on the old commit. Git's rename overwrites the destination, so
		// drop any such loose ref — except one we just wrote by moving the source remote's own refs.
		for dest in &renamed_dests {
			if loose_targets.contains(dest) {
				continue;
			}
			match self.files.delete_path(dest, None).await {
				Ok(_) | Err(FileStoreError::NotFound) => {}
				Err(other) => return Err(other.into()),
			}
		}
		Ok(())
	}

	/// Move every file under `old` to the same relative path under `new`, passing each file's bytes
	/// through `rewrite`. Descends into subdirectories the way [`Self::delete_files_under`] does.
	///
	/// All bytes are buffered first, then every target is written, then each source that is not itself
	/// a target is deleted. Writing before deleting keeps the move rollback-safe: if a write fails
	/// (e.g. a directory/file conflict with a stale ref already in the destination namespace), no
	/// source has been removed yet, so every ref still exists — matching git, which never drops a
	/// source ref whose destination it could not create. Skipping the delete of a source that is also
	/// a target keeps an overlapping rename (`new` nested under `old`, e.g. `.../origin/` →
	/// `.../origin/foo/`) from deleting a ref it just wrote.
	async fn move_files_under(
		&self,
		old: &str,
		new: &str,
		rewrite: impl Fn(&[u8]) -> Vec<u8>,
	) -> Result<Vec<String>, RepositoryError> {
		let mut stack = vec![old.to_owned()];
		let mut moves: Vec<(String, Vec<u8>)> = Vec::new();
		let mut sources: Vec<String> = Vec::new();
		while let Some(dir) = stack.pop() {
			for path in self.files.list_prefix(&dir).await? {
				match self.files.read_path(&path).await {
					Ok(bytes) => {
						let target = format!("{new}{}", &path[old.len()..]);
						moves.push((target, rewrite(&bytes)));
						sources.push(path);
					}
					Err(_) => stack.push(format!("{path}/")),
				}
			}
		}
		let targets: Vec<String> = moves.iter().map(|(target, _)| target.clone()).collect();
		for (target, bytes) in &moves {
			self.force_write(target, bytes).await?;
		}
		let target_set: std::collections::HashSet<&str> = targets.iter().map(String::as_str).collect();
		for path in &sources {
			if target_set.contains(path.as_str()) {
				continue;
			}
			match self.files.delete_path(path, None).await {
				Ok(_) | Err(FileStoreError::NotFound) => {}
				Err(other) => return Err(other.into()),
			}
		}
		Ok(targets)
	}

	/// Rewrite `packed-refs`, renaming every entry whose name is under `old` to sit under `new`, and
	/// return the renamed destination names. The file is rebuilt **sorted by ref name** (each entry
	/// keeps its `^<peeled>` continuation), because renaming can move a name to a different lexical
	/// position and `packed-refs` must stay sorted (`git fsck --strict` rejects `packedRefUnsorted`).
	///
	/// A renamed entry landing on a name that already exists (a stale destination in `packed-refs`)
	/// **overwrites** it, as git's rename does — so the rebuilt file never carries a duplicate name
	/// (which `git fsck --strict` would also reject).
	async fn rename_packed_prefix(
		&self,
		old: &str,
		new: &str,
	) -> Result<Vec<String>, RepositoryError> {
		let Some(bytes) = self.read_opt("packed-refs").await? else {
			return Ok(Vec::new());
		};
		let text = std::str::from_utf8(&bytes)
			.map_err(|_| RepositoryError::InvalidRef("packed-refs not UTF-8".to_owned()))?;

		// Header/comment lines are preserved at the top; each ref line (plus any `^peeled` line) is
		// collected as one entry keyed by its (possibly renamed) name, tagged with whether it was
		// renamed so a collision can resolve in the renamed entry's favour.
		let mut header = String::new();
		let mut entries: Vec<(String, String, bool)> = Vec::new();
		let mut renamed_dests: Vec<String> = Vec::new();
		let mut changed = false;
		let mut lines = text.lines().peekable();
		while lines.peek().is_some_and(|line| line.starts_with('#')) {
			header.push_str(lines.next().unwrap());
			header.push('\n');
		}
		while let Some(line) = lines.next() {
			let Some((oid, name)) = line.split_once(' ') else {
				continue; // a stray `^`/blank line with no owning entry — drop it
			};
			let (name, renamed) = match name.strip_prefix(old) {
				Some(rest) => {
					changed = true;
					let dest = format!("{new}{rest}");
					renamed_dests.push(dest.clone());
					(dest, true)
				}
				None => (name.to_owned(), false),
			};
			let mut entry = format!("{oid} {name}\n");
			if lines.peek().is_some_and(|next| next.starts_with('^')) {
				entry.push_str(lines.next().unwrap());
				entry.push('\n');
			}
			entries.push((name, entry, renamed));
		}
		if !changed {
			return Ok(Vec::new());
		}

		// Collapse duplicate names: a renamed entry overwrites a pre-existing destination entry with
		// the same name (either arrival order), so at most one entry survives per name.
		let mut chosen: Vec<(String, String)> = Vec::new();
		let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
		for (name, entry, renamed) in entries {
			match index.get(&name) {
				Some(&i) if renamed => chosen[i].1 = entry,
				Some(_) => {}
				None => {
					index.insert(name.clone(), chosen.len());
					chosen.push((name, entry));
				}
			}
		}
		chosen.sort_by(|a, b| a.0.cmp(&b.0));

		let mut out = header;
		for (_, entry) in &chosen {
			out.push_str(entry);
		}
		self.force_write("packed-refs", out.as_bytes()).await?;
		Ok(renamed_dests)
	}

	/// Delete every file under `prefix`, descending into subdirectories the way [`Self::list`] walks:
	/// a `read_path` that fails (a real subdirectory, or a synthetic directory entry a backend like
	/// `MemoryFileStore` returns as `NotFound`) is treated as a subtree to descend into, not a file.
	async fn delete_files_under(&self, prefix: &str) -> Result<(), RepositoryError> {
		let mut stack = vec![prefix.to_owned()];
		while let Some(dir) = stack.pop() {
			for path in self.files.list_prefix(&dir).await? {
				match self.files.read_path(&path).await {
					Ok(_) => match self.files.delete_path(&path, None).await {
						Ok(_) | Err(FileStoreError::NotFound) => {}
						Err(other) => return Err(other.into()),
					},
					Err(_) => stack.push(format!("{path}/")),
				}
			}
		}
		Ok(())
	}

	/// Rewrite `packed-refs` without `name` (and its `^<peeled>` continuation line).
	/// A no-op if there is no packed-refs file or the ref is not packed.
	async fn remove_from_packed(&self, name: &str) -> Result<(), RepositoryError> {
		self.remove_packed_matching(|refname| refname == name).await
	}

	/// Rewrite `packed-refs` without the entries `drop` selects (and their `^<peeled>` continuations).
	/// A no-op if there is no packed-refs file or nothing matches.
	async fn remove_packed_matching(
		&self,
		drop: impl Fn(&str) -> bool,
	) -> Result<(), RepositoryError> {
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
				&& drop(refname)
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
	use gitana_file_store::{FileStore, FileStoreError};
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
	async fn remove_prefix_deletes_nested_refs_reflogs_and_packed() {
		let files = MemoryFileStore::new();
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");
		let main = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"main");
		// A nested direct ref, a symbolic ref, and a reflog under the remote's tree.
		let put =
			async |path: &str, bytes: &[u8]| files.write_path_if_absent(path, bytes).await.unwrap();
		put(
			"refs/remotes/origin/feature/x",
			format!("{}\n", tip.to_hex()).as_bytes(),
		)
		.await;
		put(
			"refs/remotes/origin/HEAD",
			b"ref: refs/remotes/origin/feature/x\n",
		)
		.await;
		put(
			"logs/refs/remotes/origin/feature/x",
			b"0 1 C <c@e> 0 +0000\tfetch\n",
		)
		.await;
		// A packed entry for the remote, plus an unrelated one that must survive.
		put(
			"packed-refs",
			format!(
				"# pack-refs with: peeled fully-peeled sorted\n{} refs/remotes/origin/feature/x\n{} refs/heads/main\n",
				tip.to_hex(),
				main.to_hex()
			)
			.as_bytes(),
		)
		.await;

		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		store.remove_prefix("refs/remotes/origin/").await.unwrap();

		for gone in [
			"refs/remotes/origin/feature/x",
			"refs/remotes/origin/HEAD",
			"logs/refs/remotes/origin/feature/x",
		] {
			assert!(!files.exists(gone).await.unwrap(), "{gone} deleted");
		}
		// The packed remote ref is gone; the unrelated head survives.
		let packed = String::from_utf8(files.read_path("packed-refs").await.unwrap()).unwrap();
		assert!(
			!packed.contains("refs/remotes/origin"),
			"packed remote ref removed: {packed}"
		);
		assert!(
			packed.contains("refs/heads/main"),
			"unrelated packed ref kept: {packed}"
		);
	}

	#[tokio::test]
	async fn rename_prefix_moves_refs_reflogs_and_rewrites_symbolic_targets() {
		let files = MemoryFileStore::new();
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");
		let put =
			async |path: &str, bytes: &[u8]| files.write_path_if_absent(path, bytes).await.unwrap();
		put(
			"refs/remotes/origin/main",
			format!("{}\n", tip.to_hex()).as_bytes(),
		)
		.await;
		put(
			"refs/remotes/origin/HEAD",
			b"ref: refs/remotes/origin/main\n",
		)
		.await;
		put(
			"logs/refs/remotes/origin/main",
			b"0 1 C <c@e> 0 +0000\tfetch\n",
		)
		.await;

		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		store
			.rename_prefix("refs/remotes/origin/", "refs/remotes/upstream/")
			.await
			.unwrap();

		// The old tree is gone.
		for gone in [
			"refs/remotes/origin/main",
			"refs/remotes/origin/HEAD",
			"logs/refs/remotes/origin/main",
		] {
			assert!(!files.exists(gone).await.unwrap(), "{gone} moved away");
		}
		// The new tree is present; the direct ref and reflog carried over, and the symbolic target was
		// rewritten to point under the new prefix.
		assert_eq!(
			files.read_path("refs/remotes/upstream/main").await.unwrap(),
			format!("{}\n", tip.to_hex()).into_bytes()
		);
		assert_eq!(
			files.read_path("refs/remotes/upstream/HEAD").await.unwrap(),
			b"ref: refs/remotes/upstream/main\n"
		);
		assert!(
			files
				.exists("logs/refs/remotes/upstream/main")
				.await
				.unwrap()
		);
	}

	#[tokio::test]
	async fn rename_prefix_keeps_packed_refs_sorted() {
		let files = MemoryFileStore::new();
		let aaa = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"aaa");
		let zzz = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"zzz");
		let peeled = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"peeled");
		files
			.write_path_if_absent(
				"packed-refs",
				format!(
					"# pack-refs with: peeled fully-peeled sorted\n{} refs/remotes/aaa/main\n{} refs/remotes/zzz/main\n^{}\n",
					aaa.to_hex(),
					zzz.to_hex(),
					peeled.to_hex()
				)
				.as_bytes(),
			)
			.await
			.unwrap();

		// Rename zzz → 000, which sorts before aaa: the rebuilt file must be re-sorted, and the moved
		// entry must keep its `^<peeled>` continuation.
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		store
			.rename_prefix("refs/remotes/zzz/", "refs/remotes/000/")
			.await
			.unwrap();

		let packed = String::from_utf8(files.read_path("packed-refs").await.unwrap()).unwrap();
		assert_eq!(
			packed,
			format!(
				"# pack-refs with: peeled fully-peeled sorted\n{} refs/remotes/000/main\n^{}\n{} refs/remotes/aaa/main\n",
				zzz.to_hex(),
				peeled.to_hex(),
				aaa.to_hex()
			)
		);
	}

	#[tokio::test]
	async fn rename_prefix_overwrites_stale_destination_refs() {
		let files = MemoryFileStore::new();
		let main = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"main");
		let dev = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"dev");
		let stale_packed = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"stale packed");
		let stale_loose = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"stale loose");
		// origin's tracking refs are packed; the upstream namespace holds leftover stale refs — one
		// packed (`upstream/main`) that origin/main will land on, one loose (`upstream/dev`) that would
		// shadow the renamed packed origin/dev.
		files
			.write_path_if_absent(
				"packed-refs",
				format!(
					"# pack-refs with: peeled fully-peeled sorted\n{} refs/remotes/origin/dev\n{} refs/remotes/origin/main\n{} refs/remotes/upstream/main\n",
					dev.to_hex(),
					main.to_hex(),
					stale_packed.to_hex()
				)
				.as_bytes(),
			)
			.await
			.unwrap();
		files
			.write_path_if_absent(
				"refs/remotes/upstream/dev",
				format!("{}\n", stale_loose.to_hex()).as_bytes(),
			)
			.await
			.unwrap();

		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		store
			.rename_prefix("refs/remotes/origin/", "refs/remotes/upstream/")
			.await
			.unwrap();

		// The renamed entries overwrite the stale packed destination (no duplicate `upstream/main`), and
		// the file stays sorted.
		let packed = String::from_utf8(files.read_path("packed-refs").await.unwrap()).unwrap();
		assert_eq!(
			packed,
			format!(
				"# pack-refs with: peeled fully-peeled sorted\n{} refs/remotes/upstream/dev\n{} refs/remotes/upstream/main\n",
				dev.to_hex(),
				main.to_hex()
			)
		);
		// The stale loose ref that would have shadowed the renamed packed `upstream/dev` is gone.
		assert!(matches!(
			files.read_path("refs/remotes/upstream/dev").await,
			Err(FileStoreError::NotFound)
		));
	}

	#[tokio::test]
	async fn rename_prefix_handles_a_target_nested_under_the_source() {
		let files = MemoryFileStore::new();
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");
		let b = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"b");
		let put =
			async |path: &str, bytes: &[u8]| files.write_path_if_absent(path, bytes).await.unwrap();
		put(
			"refs/remotes/origin/main",
			format!("{}\n", a.to_hex()).as_bytes(),
		)
		.await;
		put(
			"refs/remotes/origin/foo/main",
			format!("{}\n", b.to_hex()).as_bytes(),
		)
		.await;

		// Rename origin → origin/foo: the destination nests under the source, so a target overlaps a
		// source path. Both refs must survive the move.
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		store
			.rename_prefix("refs/remotes/origin/", "refs/remotes/origin/foo/")
			.await
			.unwrap();

		assert_eq!(
			files
				.read_path("refs/remotes/origin/foo/main")
				.await
				.unwrap(),
			format!("{}\n", a.to_hex()).into_bytes()
		);
		assert_eq!(
			files
				.read_path("refs/remotes/origin/foo/foo/main")
				.await
				.unwrap(),
			format!("{}\n", b.to_hex()).into_bytes()
		);
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

	#[tokio::test]
	async fn update_ref_cas_sees_packed_only_refs() {
		let files = MemoryFileStore::new();
		let packed = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"packed tip");
		let new = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"new tip");
		files
			.write_path_if_absent(
				"packed-refs",
				format!(
					"# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/packed\n",
					packed.to_hex()
				)
				.as_bytes(),
			)
			.await
			.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);

		// "Must be absent" refuses a ref that exists (packed-only)…
		assert!(matches!(
			store.update_ref("refs/heads/packed", new, None).await,
			Err(crate::RepositoryError::RefMoved { .. })
		));
		// …a wrong expected value refuses…
		assert!(matches!(
			store.update_ref("refs/heads/packed", new, Some(new)).await,
			Err(crate::RepositoryError::RefMoved { .. })
		));
		// …and the packed value is the compare value: the update writes the
		// shadowing loose file.
		store
			.update_ref("refs/heads/packed", new, Some(packed))
			.await
			.expect("CAS over the packed value");
		assert_eq!(
			store.resolve("refs/heads/packed").await.expect("resolve"),
			Some(new)
		);
		assert!(
			files.read_path("refs/heads/packed").await.is_ok(),
			"a loose file now shadows the packed entry"
		);
	}
}
