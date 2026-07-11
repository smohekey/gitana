use std::marker::PhantomData;

use gitana_file_store::{FileStore, FileStoreError, WriteOutcome};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{HeadState, RefOp, RepositoryError};

/// The maximum symbolic-ref chain depth to follow (git's limit), a guard against a cycle.
const MAX_SYMREF_DEPTH: usize = 5;

/// How many times to retry acquiring a contended `<ref>.lock`, and the wait between tries — mirrors
/// the file store's own `LockFileGuard` (50 × 10 ms), so a ref transaction waits for stock git (or
/// another gitana writer) to release the lock instead of failing instantly.
const LOCK_ATTEMPTS: usize = 50;
#[cfg(not(target_arch = "wasm32"))]
const LOCK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

/// Wait before retrying a contended ref lock. Native sleeps on the blocking pool (keeping the reactor
/// free without a tokio timer feature), so it actually waits out a cross-process holder; wasm is
/// single-process, so a cooperative yield — letting the in-runtime lock holder progress — suffices.
#[cfg(not(target_arch = "wasm32"))]
async fn lock_backoff() {
	let _ = tokio::task::spawn_blocking(|| std::thread::sleep(LOCK_BACKOFF)).await;
}
#[cfg(target_arch = "wasm32")]
async fn lock_backoff() {
	use std::future::Future;
	use std::pin::Pin;
	use std::task::{Context, Poll};

	struct YieldOnce(bool);
	impl Future for YieldOnce {
		type Output = ();
		fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
			if std::mem::replace(&mut self.0, true) {
				Poll::Ready(())
			} else {
				cx.waker().wake_by_ref();
				Poll::Pending
			}
		}
	}
	YieldOnce(false).await
}

/// Whether a ref update should append a reflog entry, and with what identity and message.
///
/// A required argument on the ref-moving methods ([`RefStore::update_ref`],
/// [`RefStore::set_symbolic`]) so a call site cannot silently forget to decide. `Log` *requests* a
/// reflog line but the write is still subject to git's `core.logAllRefUpdates` gating (namespace and
/// bare-repo rules); `Skip` never writes one — for internal or plumbing moves git does not log.
#[derive(Clone, Copy)]
pub enum ReflogIntent<'a> {
	/// Append a reflog entry (when gating permits) crediting `committer` with `message`. An empty
	/// `message` records a line with no message (git omits the tab), as `git update-ref` does without
	/// `-m`.
	Log {
		/// The reflog committer line (`Name <email> seconds ±hhmm`).
		committer: &'a str,
		/// The reflog message (e.g. `branch: Created from HEAD`), or `""` for none.
		message: &'a str,
	},
	/// Do not write a reflog entry.
	Skip,
}

/// git's `core.logAllRefUpdates` policy, resolved from a repository's config.
#[derive(Clone, Copy)]
enum ReflogPolicy {
	/// Log every ref under `refs/` (config `always`).
	Always,
	/// Log the standard namespaces (`HEAD`, `refs/heads/*`, `refs/remotes/*`, `refs/notes/*`) and any
	/// ref that already has a log (config `true`, or unset in a non-bare repo).
	Enabled,
	/// Log only refs that already have a log file (config `false`, or unset in a bare repo).
	Disabled,
}

/// Reads and updates refs (loose files + symbolic HEAD) over a file store.
///
/// Borrows the repository's file store and id, so it shares the one backend the
/// object store already holds. packed-refs reading and the reflog land in later
/// phases (see docs/hlds/repository-engine.md). Generic over the hash algorithm `H`,
/// which fixes the width of the object ids refs resolve to.
pub struct RefStore<'a, F, H> {
	files: &'a F,
	/// The effective (merged) config lent by [`Repository::refs`], borrowed for the store's
	/// lifetime. `reflog_policy` reads `core.logallrefupdates` from it so a global/system setting is
	/// honoured; `None` (a store built directly over a file store, as in tests) falls back to the
	/// raw-local `config` file.
	effective: Option<&'a gitana_config::GitConfig>,
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
			effective: None,
			_hash: PhantomData,
		}
	}

	/// Lend the store the effective (merged) config for its reflog-policy read. Called by
	/// [`Repository::refs`]; a `None` leaves the store on the raw-local `config` fallback.
	pub fn with_effective_config(mut self, effective: Option<&'a gitana_config::GitConfig>) -> Self {
		self.effective = effective;
		self
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

	/// Resolve a ref to an object id, following a bounded chain of symbolic (`ref:`) refs — so a
	/// remote's symbolic `HEAD` (`refs/remotes/origin/HEAD` → `ref: refs/remotes/origin/main`) resolves
	/// to the branch it names. `None` if the ref or its chain does not resolve. Unlike [`Self::resolve`],
	/// which parses a ref's body as a hex oid, this accepts and follows a symbolic ref.
	pub async fn resolve_symbolic(&self, name: &str) -> Result<Option<ObjectId<H>>, RepositoryError> {
		self.follow_symref(name).await
	}

	/// Compare-and-set a ref. `expected == None` requires the ref to be absent; otherwise the current
	/// value must equal `expected`. A ref present only in `packed-refs` counts as its packed value —
	/// updating it writes the loose file, which shadows the packed entry from then on (as git does).
	///
	/// A one-op [`transact`](Self::transact): the ref is locked, its reflog written, then the ref
	/// committed — so a reflog-write failure (or a lost race) leaves the ref unmoved.
	pub async fn update_ref(
		&self,
		name: &str,
		new: ObjectId<H>,
		expected: Option<ObjectId<H>>,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		let op = RefOp {
			name: name.to_owned(),
			expected,
			new: Some(new),
			reflog,
		};
		self
			.transact(std::slice::from_ref(&op))
			.await
			.map_err(|(_, error)| error)
	}

	/// Delete a ref, requiring its current resolved value to equal `expected` (CAS).
	///
	/// Removes the loose ref file (if any), drops the ref from `packed-refs` (if present), and deletes
	/// its reflog, so the ref no longer resolves by either path and leaves no stale log — as git does.
	/// Errors with [`RepositoryError::RefMoved`] if the current value differs from `expected`, and
	/// [`RepositoryError::InvalidRef`] if there is no such ref.
	///
	/// `reflog` mirrors [`update_ref`](Self::update_ref)'s split-HEAD cascade for a deletion: with
	/// `Log`, deleting the branch `HEAD` points at appends a `<old> <zero>` entry to `logs/HEAD`
	/// (subject to `core.logAllRefUpdates` gating), as git's receive-pack does when it removes the
	/// current branch. A one-op [`transact`](Self::transact): the reflog is written before the ref is
	/// removed, under the ref's lock, so a failure rejects without having touched anything.
	pub async fn delete_ref(
		&self,
		name: &str,
		expected: Option<ObjectId<H>>,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		let op = RefOp {
			name: name.to_owned(),
			expected,
			new: None,
			reflog,
		};
		self
			.transact(std::slice::from_ref(&op))
			.await
			.map_err(|(_, error)| error)
	}

	/// Apply `ops` as one atomic ref transaction — git's ref-lock model.
	///
	/// Every op's ref (and `HEAD`, for a split-HEAD reflog cascade) is locked via `<ref>.lock`,
	/// acquired in a fixed sorted order so concurrent transactions cannot deadlock; every precondition
	/// is validated; then each op's reflog is written and its ref committed. Any failure applies
	/// nothing and returns the offending ref name and error.
	///
	/// Because an op writes its reflog *before* its ref while holding the lock, a reflog-write failure
	/// or a lost CAS race leaves the ref untouched — the atomicity a raw ref move lacked.
	/// [`update_ref`](Self::update_ref) and [`delete_ref`](Self::delete_ref) are one-op wrappers; a
	/// caller wanting all-or-nothing across several refs (a `--atomic` push) passes them together.
	pub async fn transact(&self, ops: &[RefOp<'_, H>]) -> Result<(), (String, RepositoryError)> {
		let anon = |error| (String::new(), error);
		let policy = self.reflog_policy().await.map_err(anon)?;
		// The branch HEAD points at (read once, before locking): an op on it cascades into `logs/HEAD`,
		// so HEAD joins the lock set.
		let head_target = self.read_symbolic("HEAD").await.map_err(anon)?;
		let cascades: Vec<bool> = ops
			.iter()
			.map(|op| op.name.starts_with("refs/heads/") && head_target.as_deref() == Some(&op.name))
			.collect();

		let mut lock_names: Vec<String> = ops.iter().map(|op| op.name.clone()).collect();
		if cascades.iter().any(|&c| c) {
			lock_names.push("HEAD".to_owned());
		}
		let acquired = self.lock_all(&lock_names).await?;

		// Confirm the cascade under the acquired locks (catching a `HEAD` retarget in the
		// pre-lock→lock window), then validate and commit — releasing every lock on any path.
		let outcome = match self.confirm_cascades(ops, cascades).await {
			Ok(cascades) => self.transact_locked(ops, &cascades, policy).await,
			Err(error) => Err(error),
		};

		for name in &acquired {
			self.unlock_ref(name).await;
		}
		outcome
	}

	/// Re-derive the HEAD cascade flags under the acquired locks. `HEAD` is read once *before* locking
	/// to fix the lock set (it is locked iff some op cascades); re-reading it here — while we hold
	/// `HEAD.lock` when it matters — catches a concurrent `set_symbolic` in the pre-lock→lock window.
	/// An op then cascades only when we hold `HEAD.lock` **and** `HEAD` still points at it, so the
	/// transaction never appends to `logs/HEAD` without holding `HEAD.lock`.
	async fn confirm_cascades(
		&self,
		ops: &[RefOp<'_, H>],
		cascades: Vec<bool>,
	) -> Result<Vec<bool>, (String, RepositoryError)> {
		if !cascades.iter().any(|&c| c) {
			// No op cascaded pre-lock, so `HEAD` was not locked; leave the flags off rather than trust a
			// fresh read we could not act on safely.
			return Ok(cascades);
		}
		let head = self
			.read_symbolic("HEAD")
			.await
			.map_err(|error| (String::new(), error))?;
		Ok(
			ops
				.iter()
				.zip(cascades)
				.map(|(op, cascade)| cascade && head.as_deref() == Some(&op.name))
				.collect(),
		)
	}

	/// Validate every op, then commit every op — assuming the full lock set is held. Split out so
	/// [`transact`](Self::transact) releases the locks on every return path.
	async fn transact_locked(
		&self,
		ops: &[RefOp<'_, H>],
		cascades: &[bool],
		policy: ReflogPolicy,
	) -> Result<(), (String, RepositoryError)> {
		// Validate all preconditions before mutating anything — so the common rejections (a stale
		// `expected`, deleting a missing ref) apply nothing, even in a multi-op transaction.
		let mut olds = Vec::with_capacity(ops.len());
		for (op, &cascade) in ops.iter().zip(cascades) {
			let current = self
				.resolve(&op.name)
				.await
				.map_err(|e| (op.name.clone(), e))?;
			if current != op.expected {
				return Err((
					op.name.clone(),
					RepositoryError::RefMoved {
						name: op.name.clone(),
					},
				));
			}
			if op.new.is_none() && current.is_none() {
				return Err((
					op.name.clone(),
					RepositoryError::InvalidRef(format!("{}: no such ref", op.name)),
				));
			}
			// Preflight every directory/file conflict a commit could otherwise hit — so a validated
			// transaction cannot fail at commit (bar catastrophic I/O), keeping even a multi-op
			// `--atomic` batch all-or-nothing. Which reflog paths get written:
			//   - a move (`new` set) writes the ref and, when logged, its branch reflog;
			//   - a move *or a delete* that cascades writes the mirrored `logs/HEAD`.
			let mut logged: Vec<&str> = Vec::new();
			if op.new.is_some() {
				if self
					.path_write_blocked(&op.name)
					.await
					.map_err(|e| (op.name.clone(), e))?
				{
					return Err((
						op.name.clone(),
						RepositoryError::InvalidRef(format!(
							"{}: blocked by an existing directory or file",
							op.name
						)),
					));
				}
				// The *direct* branch reflog is skipped for a no-op (`current == new`), matching
				// `log_ref_update` — so don't preflight it there either, or a no-op update would be
				// rejected over a `logs/<ref>` conflict the commit never touches. (The HEAD cascade below
				// is still written for a no-op, so it is not gated this way.)
				if matches!(op.reflog, ReflogIntent::Log { .. })
					&& op.new != current
					&& self
						.should_log(&op.name, policy)
						.await
						.map_err(|e| (op.name.clone(), e))?
				{
					logged.push(&op.name);
				}
			}
			if matches!(op.reflog, ReflogIntent::Log { .. })
				&& cascade
				&& self
					.should_log("HEAD", policy)
					.await
					.map_err(|e| (op.name.clone(), e))?
			{
				logged.push("HEAD");
			}
			for name in logged {
				if self
					.path_write_blocked(&format!("logs/{name}"))
					.await
					.map_err(|e| (op.name.clone(), e))?
				{
					return Err((
						op.name.clone(),
						RepositoryError::InvalidRef(format!(
							"{}: reflog path {name} blocked by an existing file or directory",
							op.name
						)),
					));
				}
			}
			olds.push(current);
		}

		// Commit each op. Validation preflighted every directory/file conflict, so a commit can now
		// fail only on catastrophic I/O — nothing else moves a ref and then reports failure.
		for ((op, &old), &cascade) in ops.iter().zip(&olds).zip(cascades) {
			self
				.commit_op(op, old, cascade, policy)
				.await
				.map_err(|e| (op.name.clone(), e))?;
		}
		Ok(())
	}

	/// Commit one validated op under its held lock: the ref, then its reflog(s).
	async fn commit_op(
		&self,
		op: &RefOp<'_, H>,
		old: Option<ObjectId<H>>,
		cascade: bool,
		policy: ReflogPolicy,
	) -> Result<(), RepositoryError> {
		match op.new {
			Some(new) => {
				// Reflog first, then the ref. Validation preflighted both paths, so neither write can hit
				// a directory/file conflict; writing the reflog first means that even a catastrophic
				// backend failure on `logs/` leaves the ref unpublished, so a reported failure never
				// advances the branch (receive-pack relies on this). The HEAD cascade uses the prepared
				// `cascade` (HEAD was locked accordingly), not a fresh read.
				if let ReflogIntent::Log { committer, message } = op.reflog {
					self
						.log_ref_update(&op.name, old, new, committer, message, cascade, policy)
						.await?;
				}
				// We hold the lock and validated the value, so a plain replace commits the move — no CAS,
				// and no `<ref>.lock` of its own to deadlock against ours.
				self
					.files
					.write_path_replace(&op.name, format!("{new}\n").as_bytes())
					.await?;
			}
			None => {
				// Deletion: mirror the `<old> <zero>` HEAD entry (before removing anything), then remove
				// the loose ref, its packed entry, and its own reflog.
				if let ReflogIntent::Log { committer, message } = op.reflog
					&& cascade
					&& self.should_log("HEAD", policy).await?
				{
					self
						.append_reflog("HEAD", old, None, committer, message)
						.await?;
				}
				self.files.delete_path_unlocked(&op.name).await?;
				self.remove_from_packed(&op.name).await?;
				// Best-effort, like git: a stale reflog (e.g. a leftover `logs/<name>` directory) must
				// not turn a completed deletion into a reported failure. Prune the reflog's now-empty
				// parent dirs too (the ref's own are pruned when its lock is released), so a later ref
				// there is not blocked by a leftover `logs/` directory.
				let logs = format!("logs/{}", op.name);
				let _ = self.files.delete_path_unlocked(&logs).await;
				self.prune_empty_dirs(&logs).await;
			}
		}
		Ok(())
	}

	/// Whether writing a value at `target` would hit a directory/file conflict: `target` is itself a
	/// directory (a leftover from a nested ref/reflog, e.g. `refs/heads/foo` when `refs/heads/foo/bar`
	/// exists, or an empty dir a delete left behind), or a strict ancestor is a *file* (blocking the
	/// intermediate directory, e.g. a stray `logs/refs/heads/foo` file under `logs/refs/heads/foo/bar`).
	///
	/// A transaction preflights this for a move's ref path and its reflog path, so a validated commit
	/// cannot fail on such a conflict. `is_dir` catches the directory case (including empty dirs);
	/// `read_path` catches a file ancestor — it reads back `Ok` only for a file (a directory or absent
	/// path errors, with a backend-varying kind, so we key on `Ok`).
	async fn path_write_blocked(&self, target: &str) -> Result<bool, RepositoryError> {
		if self.files.is_dir(target).await? {
			return Ok(true);
		}
		for (index, _) in target.match_indices('/') {
			if self.files.read_path(&target[..index]).await.is_ok() {
				return Ok(true);
			}
		}
		Ok(false)
	}

	/// Acquire every `<name>.lock` in `names`, sorted and deduped so concurrent transactions take
	/// shared locks in the same order (deadlock-free). On the first contended lock, releases those
	/// already taken and reports it.
	async fn lock_all(&self, names: &[String]) -> Result<Vec<String>, (String, RepositoryError)> {
		let mut sorted: Vec<String> = names.to_vec();
		sorted.sort();
		sorted.dedup();
		let mut acquired: Vec<String> = Vec::with_capacity(sorted.len());
		for name in sorted {
			if let Err(error) = self.lock_ref(&name).await {
				for held in &acquired {
					self.unlock_ref(held).await;
				}
				return Err((name, error));
			}
			acquired.push(name);
		}
		Ok(acquired)
	}

	/// Take `<name>.lock` (git's ref lock), retrying briefly on contention before giving up with
	/// [`RepositoryError::RefLocked`].
	async fn lock_ref(&self, name: &str) -> Result<(), RepositoryError> {
		let path = format!("{name}.lock");
		for attempt in 0..LOCK_ATTEMPTS {
			match self.files.write_path_if_absent(&path, &[]).await? {
				WriteOutcome::Written => return Ok(()),
				WriteOutcome::AlreadyExists => {
					if attempt + 1 < LOCK_ATTEMPTS {
						lock_backoff().await;
					}
				}
			}
		}
		Err(RepositoryError::RefLocked {
			name: name.to_owned(),
		})
	}

	/// Release `<name>.lock`, using the lock-free unlink so it never contends for the very lock it is
	/// removing, then prune any now-empty parent directories.
	async fn unlock_ref(&self, name: &str) {
		let _ = self
			.files
			.delete_path_unlocked(&format!("{name}.lock"))
			.await;
		// Acquiring `<name>.lock` may have created `<name>`'s parent directories (e.g. `refs/heads/foo/`
		// for `refs/heads/foo/bar.lock`); an aborted transaction, or a delete that emptied the tree,
		// leaves them behind. Git prunes such empty ref directories so a stale `refs/heads/foo/` cannot
		// masquerade as a directory/file conflict blocking a later `refs/heads/foo`.
		self.prune_empty_dirs(name).await;
	}

	/// Best-effort removal of `path`'s now-empty ancestor directories, from the innermost up, stopping
	/// at the first that is not an empty directory (or on any error / a backend without directories).
	async fn prune_empty_dirs(&self, path: &str) {
		let mut current = path;
		while let Some(index) = current.rfind('/') {
			let parent = &current[..index];
			if parent.is_empty() || self.files.remove_dir(parent).await.is_err() {
				break;
			}
			current = parent;
		}
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
	pub async fn set_head_symbolic(
		&self,
		target: &str,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		self.set_symbolic("HEAD", target, reflog).await
	}

	/// Point the symbolic ref `name` (e.g. `HEAD`) at `target`.
	///
	/// Retargeting a symbolic ref moves the object it resolves to, so — like git — it appends a reflog
	/// entry to `name` (from the pre-retarget resolved value to `target`'s), subject to the
	/// [`ReflogIntent`] and `core.logAllRefUpdates` gating. Skipped when `target` does not yet resolve
	/// (e.g. `HEAD` pointed at an unborn branch), which has no object movement to record.
	pub async fn set_symbolic(
		&self,
		name: &str,
		target: &str,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		// Hold `<name>.lock` across the reflog write and the retarget — like a ref transaction, so a
		// reflog failure leaves the symbolic ref unchanged and no concurrent writer interleaves.
		self.lock_ref(name).await?;
		let result = self.set_symbolic_locked(name, target, reflog).await;
		self.unlock_ref(name).await;
		result
	}

	/// The body of [`set_symbolic`](Self::set_symbolic), run with `<name>.lock` held.
	async fn set_symbolic_locked(
		&self,
		name: &str,
		target: &str,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		// Preflight the destination's writability before appending any reflog (as `transact` does): a
		// directory/file conflict at `name` (or, when logged, at `logs/<name>`) must reject the retarget
		// rather than record a reflog for a move that then fails on the ref write.
		if self.path_write_blocked(name).await? {
			return Err(RepositoryError::InvalidRef(format!(
				"{name}: blocked by an existing directory or file"
			)));
		}
		let old = self.follow_symref(name).await?;
		// Reflog first (before retargeting), gated, and only when `target` resolves — no object
		// movement to record otherwise.
		if let ReflogIntent::Log { committer, message } = reflog
			// Follow the chain: `target` may itself be symbolic (e.g. `refs/remotes/origin/HEAD`), which
			// `resolve` would try to parse as an object id and reject.
			&& let Some(new) = self.follow_symref(target).await?
			&& self.should_log(name, self.reflog_policy().await?).await?
		{
			if self.path_write_blocked(&format!("logs/{name}")).await? {
				return Err(RepositoryError::InvalidRef(format!(
					"{name}: reflog path blocked by an existing file or directory"
				)));
			}
			self
				.append_reflog(name, old, Some(new), committer, message)
				.await?;
		}
		// Commit the retarget under the held lock (a plain replace, no `<name>.lock` of its own).
		let bytes = HeadState::<H>::Symbolic(target.to_owned()).render();
		self
			.files
			.write_path_replace(name, bytes.as_bytes())
			.await?;
		Ok(())
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
	/// ref lock: `<old> <new> <committer>\t<message>\n` to `logs/<refname>`. An empty `message`
	/// records `<old> <new> <committer>\n` with no tab, matching git (`log_ref_write_fd` adds the
	/// tab and message only when the message is non-empty). A `None` `old` (creation) or `new`
	/// (deletion) renders the all-zero id git writes for that side.
	pub async fn append_reflog(
		&self,
		refname: &str,
		old: Option<ObjectId<H>>,
		new: Option<ObjectId<H>>,
		committer: &str,
		message: &str,
	) -> Result<(), RepositoryError> {
		let zero = || "0".repeat(H::RAW_LEN * 2);
		let old = old.map_or_else(zero, |id| id.to_hex());
		let new = new.map_or_else(zero, |id| id.to_hex());
		let line = if message.is_empty() {
			format!("{old} {new} {committer}\n")
		} else {
			format!("{old} {new} {committer}\t{message}\n")
		};

		let path = format!("logs/{refname}");
		let mut content = match self.files.read_path(&path).await {
			Ok(bytes) => bytes,
			Err(FileStoreError::NotFound) => Vec::new(),
			Err(other) => return Err(other.into()),
		};
		content.extend_from_slice(line.as_bytes());
		self.force_write(&path, &content).await
	}

	/// Append the reflog line(s) for an [`update_ref`](Self::update_ref) that changed `name` from
	/// `old` to `new`, when `core.logAllRefUpdates` gating permits. When `name` is a branch that
	/// `HEAD` symbolically points at, git also mirrors the entry into `HEAD`'s reflog — the "split
	/// HEAD update" — so this cascades there too (each subject to its own gating).
	async fn log_ref_update(
		&self,
		name: &str,
		old: Option<ObjectId<H>>,
		new: ObjectId<H>,
		committer: &str,
		message: &str,
		cascade: bool,
		policy: ReflogPolicy,
	) -> Result<(), RepositoryError> {
		// git skips the direct reflog for a no-op update (the new value equals the old), logging only a
		// real move or a creation.
		if old != Some(new) && self.should_log(name, policy).await? {
			self
				.append_reflog(name, old, Some(new), committer, message)
				.await?;
		}
		// The split HEAD update mirrored into `HEAD` when it points at the branch is a distinct update
		// that git logs even for a no-op (`update-ref` to the current branch's own tip still records a
		// HEAD entry — verified against stock git), so it is not gated on `old != new`. It is gated on
		// the transaction's *prepared* `cascade` (HEAD was read and locked accordingly) — not a fresh
		// `HEAD` read here, which could race a concurrent retarget and append without `HEAD.lock`.
		if cascade && self.should_log("HEAD", policy).await? {
			self
				.append_reflog("HEAD", old, Some(new), committer, message)
				.await?;
		}
		Ok(())
	}

	/// Whether *creating* a new reflog for `name` is enabled under this repo's `core.logAllRefUpdates`
	/// (namespace + bare-repo policy), ignoring the "a reflog already exists" carve-out. Exposed for
	/// callers that write git's reflog layout directly rather than through [`Self::update_ref`] — e.g.
	/// `worktree add` materialising a new worktree's per-worktree `logs/HEAD` — so they honour the
	/// same setting.
	pub async fn creates_reflog_for(&self, name: &str) -> Result<bool, RepositoryError> {
		Ok(match self.reflog_policy().await? {
			ReflogPolicy::Always => true,
			ReflogPolicy::Enabled => is_standard_logged(name),
			ReflogPolicy::Disabled => false,
		})
	}

	/// Whether a ref update to `name` should be logged under `policy`, per git's
	/// `core.logAllRefUpdates` rules (namespace, bare-repo default, and the "a reflog already exists"
	/// carve-out).
	async fn should_log(&self, name: &str, policy: ReflogPolicy) -> Result<bool, RepositoryError> {
		match policy {
			ReflogPolicy::Always => Ok(true),
			ReflogPolicy::Enabled => Ok(is_standard_logged(name) || self.reflog_exists(name).await?),
			ReflogPolicy::Disabled => self.reflog_exists(name).await,
		}
	}

	/// Whether `logs/<name>` already exists (git always appends to an existing reflog, whatever the
	/// `core.logAllRefUpdates` setting).
	async fn reflog_exists(&self, name: &str) -> Result<bool, RepositoryError> {
		match self.files.read_path(&format!("logs/{name}")).await {
			Ok(_) => Ok(true),
			Err(FileStoreError::NotFound) => Ok(false),
			Err(other) => Err(other.into()),
		}
	}

	/// Resolve `core.logAllRefUpdates` from config: `always`, a git boolean, or — unset — git's
	/// default (on for a non-bare repo, off for a bare one). A missing or unparseable config falls
	/// back to the non-bare default, as an on-disk gitana repo is never bare-by-omission.
	///
	/// `logallrefupdates` follows git's merged precedence when the frontend installed the effective
	/// config (a global `true` enables reflogs); the `core.bare` fallback stays repo-local, matching
	/// the rest of gitana — a *global* `core.bare` is a footgun, so it is not honoured.
	async fn reflog_policy(&self) -> Result<ReflogPolicy, RepositoryError> {
		// The repo-local config: the sole source for `core.bare`, and the `logallrefupdates` source
		// when no effective (merged) config was installed (tests, the wasm sandbox).
		let local = match self.files.read_path("config").await {
			Ok(bytes) => std::str::from_utf8(&bytes)
				.ok()
				.and_then(|text| gitana_config::GitConfig::parse(text).ok()),
			Err(FileStoreError::NotFound) => None,
			Err(other) => return Err(other.into()),
		};
		let Some(config) = self.effective.or(local.as_ref()) else {
			return Ok(ReflogPolicy::Enabled);
		};
		if config
			.get_string("core", None, "logallrefupdates")
			.is_some_and(|value| value.eq_ignore_ascii_case("always"))
		{
			return Ok(ReflogPolicy::Always);
		}
		match config.get_bool("core", None, "logallrefupdates") {
			Ok(Some(true)) => Ok(ReflogPolicy::Enabled),
			Ok(Some(false)) => Ok(ReflogPolicy::Disabled),
			// Unset (or an unparseable value): git's default keys off whether the repo is bare. Read
			// `core.bare` from the local config only — a global bare is deliberately not honoured.
			_ => {
				let bare = local
					.as_ref()
					.and_then(|c| c.get_bool("core", None, "bare").ok().flatten())
					.unwrap_or(false);
				Ok(if bare {
					ReflogPolicy::Disabled
				} else {
					ReflogPolicy::Enabled
				})
			}
		}
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

/// Whether `name` is in a namespace git logs by default under `core.logAllRefUpdates=true`:
/// `HEAD`, local branches, remote-tracking refs, and notes (tags and other refs are excluded).
fn is_standard_logged(name: &str) -> bool {
	name == "HEAD"
		|| name.starts_with("refs/heads/")
		|| name.starts_with("refs/remotes/")
		|| name.starts_with("refs/notes/")
}

fn parse_oid<H: HashAlgorithm>(name: &str, bytes: &[u8]) -> Result<ObjectId<H>, RepositoryError> {
	let text = std::str::from_utf8(bytes)
		.map_err(|_| RepositoryError::InvalidRef(name.to_owned()))?
		.trim();
	ObjectId::from_hex(text).map_err(|_| RepositoryError::InvalidRef(format!("{name}: {text}")))
}

#[cfg(test)]
mod tests {
	use gitana_file_store::{FileStore, FileStoreError};
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::{ObjectId, ObjectKind, Sha256};

	use super::{RefStore, ReflogIntent};

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
			store
				.update_ref("refs/heads/packed", new, None, ReflogIntent::Skip)
				.await,
			Err(crate::RepositoryError::RefMoved { .. })
		));
		// …a wrong expected value refuses…
		assert!(matches!(
			store
				.update_ref("refs/heads/packed", new, Some(new), ReflogIntent::Skip)
				.await,
			Err(crate::RepositoryError::RefMoved { .. })
		));
		// …and the packed value is the compare value: the update writes the
		// shadowing loose file.
		store
			.update_ref("refs/heads/packed", new, Some(packed), ReflogIntent::Skip)
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

	#[tokio::test]
	async fn transact_applies_every_op_on_success() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");
		let b = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"b");
		let ops = [
			crate::RefOp {
				name: "refs/heads/one".to_owned(),
				expected: None,
				new: Some(a),
				reflog: ReflogIntent::Skip,
			},
			crate::RefOp {
				name: "refs/heads/two".to_owned(),
				expected: None,
				new: Some(b),
				reflog: ReflogIntent::Skip,
			},
		];
		store.transact(&ops).await.expect("both creates apply");
		assert_eq!(store.resolve("refs/heads/one").await.unwrap(), Some(a));
		assert_eq!(store.resolve("refs/heads/two").await.unwrap(), Some(b));
	}

	#[tokio::test]
	async fn transact_is_all_or_nothing() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");
		let b = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"b");
		store
			.update_ref("refs/heads/one", a, None, ReflogIntent::Skip)
			.await
			.unwrap();

		// Create `two` and update `one` with a *stale* expected value in one transaction: the stale op
		// rejects the whole batch, so `two` is never created and `one` is untouched.
		let ops = [
			crate::RefOp {
				name: "refs/heads/two".to_owned(),
				expected: None,
				new: Some(b),
				reflog: ReflogIntent::Skip,
			},
			crate::RefOp {
				name: "refs/heads/one".to_owned(),
				expected: Some(b),
				new: Some(b),
				reflog: ReflogIntent::Skip,
			},
		];
		let (name, error) = store
			.transact(&ops)
			.await
			.expect_err("a stale expected must reject the batch");
		assert_eq!(name, "refs/heads/one");
		assert!(matches!(error, crate::RepositoryError::RefMoved { .. }));
		assert_eq!(
			store.resolve("refs/heads/two").await.unwrap(),
			None,
			"a rejected batch creates no ref"
		);
		assert_eq!(
			store.resolve("refs/heads/one").await.unwrap(),
			Some(a),
			"a rejected batch moves no ref"
		);
	}

	#[tokio::test]
	async fn a_held_ref_lock_blocks_then_is_released() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");

		// Another writer holds refs/heads/x.lock: the update retries, then rejects, writing nothing.
		files
			.write_path_if_absent("refs/heads/x.lock", b"")
			.await
			.unwrap();
		let error = store
			.update_ref("refs/heads/x", a, None, ReflogIntent::Skip)
			.await
			.expect_err("a held lock blocks the update");
		assert!(matches!(error, crate::RepositoryError::RefLocked { .. }));
		assert_eq!(
			store.resolve("refs/heads/x").await.unwrap(),
			None,
			"nothing is written while the ref is locked"
		);

		// Release it: the update now lands and leaves no lock behind.
		files
			.delete_path_unlocked("refs/heads/x.lock")
			.await
			.unwrap();
		store
			.update_ref("refs/heads/x", a, None, ReflogIntent::Skip)
			.await
			.expect("update after the lock is released");
		assert_eq!(store.resolve("refs/heads/x").await.unwrap(), Some(a));
		assert!(
			!files.exists("refs/heads/x.lock").await.unwrap(),
			"the transaction released its own lock"
		);
	}

	/// A multi-op transaction where one op's reflog path has a directory/file conflict rejects the
	/// whole batch in validation — no ref is moved and no reflog is written. Uses `LocalFileStore`, the
	/// only backend with real directory/file semantics.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn transact_is_atomic_across_a_reflog_conflict() {
		use gitana_file_store_local::LocalFileStore;

		let tmp = std::env::temp_dir().join(format!("gitana-reftx-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(&tmp).unwrap();
		let files = LocalFileStore::from_dir(
			cap_std::fs::Dir::open_ambient_dir(&tmp, cap_std::ambient_authority()).unwrap(),
		);
		let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");
		let b = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"b");
		let who = "C O Mitter <c@e> 0 +0000";
		let log = ReflogIntent::Log {
			committer: who,
			message: "create",
		};

		// A stray *file* where refs/heads/foo/bar's reflog directory must go.
		files
			.write_path_replace("logs/refs/heads/foo", b"stray\n")
			.await
			.unwrap();

		// One transaction creating a clean ref and the conflicted one, both logging: the conflict must
		// reject the whole batch during validation, before either ref (or reflog) is written.
		let ops = [
			crate::RefOp {
				name: "refs/heads/one".to_owned(),
				expected: None,
				new: Some(a),
				reflog: log,
			},
			crate::RefOp {
				name: "refs/heads/foo/bar".to_owned(),
				expected: None,
				new: Some(b),
				reflog: log,
			},
		];
		let (name, _) = store
			.transact(&ops)
			.await
			.expect_err("a reflog directory/file conflict must reject the batch");
		assert_eq!(name, "refs/heads/foo/bar");
		assert_eq!(
			store.resolve("refs/heads/one").await.unwrap(),
			None,
			"the clean ref is not created by a rejected batch"
		);
		assert_eq!(store.resolve("refs/heads/foo/bar").await.unwrap(), None);
		assert!(
			files.read_path("logs/refs/heads/one").await.is_err(),
			"no reflog is written for a rejected batch"
		);

		let _ = std::fs::remove_dir_all(&tmp);
	}

	/// A directory/file conflict at the *ref* path (creating `refs/heads/foo` while
	/// `refs/heads/foo/bar` makes it a directory) rejects the update without writing a reflog — the
	/// ref write is committed first, so it fails before the log records a movement that never happened.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn a_ref_name_conflict_rejects_without_a_reflog() {
		use gitana_file_store_local::LocalFileStore;

		let tmp = std::env::temp_dir().join(format!("gitana-refdf-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(&tmp).unwrap();
		let files = LocalFileStore::from_dir(
			cap_std::fs::Dir::open_ambient_dir(&tmp, cap_std::ambient_authority()).unwrap(),
		);
		let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");

		// Create refs/heads/foo/bar, so refs/heads/foo is now a directory.
		store
			.update_ref("refs/heads/foo/bar", a, None, ReflogIntent::Skip)
			.await
			.unwrap();

		// Creating refs/heads/foo is a directory/file conflict at the ref path: it must reject, and
		// leave no reflog behind (the ref write fails before the reflog is appended).
		let error = store
			.update_ref(
				"refs/heads/foo",
				a,
				None,
				ReflogIntent::Log {
					committer: "C O Mitter <c@e> 0 +0000",
					message: "create",
				},
			)
			.await
			.expect_err("a ref-name conflict must reject the update");
		assert!(
			!matches!(error, crate::RepositoryError::RefMoved { .. }),
			"the rejection is the write conflict, not a CAS mismatch"
		);
		assert!(
			files.read_path("logs/refs/heads/foo").await.is_err(),
			"no reflog is written when the ref itself cannot be created"
		);
		assert_eq!(
			store.resolve("refs/heads/foo/bar").await.unwrap(),
			Some(a),
			"the pre-existing nested ref is untouched"
		);

		let _ = std::fs::remove_dir_all(&tmp);
	}

	/// A multi-op batch where a *later* op has a ref-name conflict rejects the whole batch — an earlier,
	/// clean op is not left committed. (The conflict is caught in validation, before any commit.)
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn transact_is_atomic_across_a_ref_name_conflict() {
		use gitana_file_store_local::LocalFileStore;

		let tmp = std::env::temp_dir().join(format!("gitana-refdf2-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(&tmp).unwrap();
		let files = LocalFileStore::from_dir(
			cap_std::fs::Dir::open_ambient_dir(&tmp, cap_std::ambient_authority()).unwrap(),
		);
		let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");
		let b = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"b");

		// refs/heads/foo is a directory (refs/heads/foo/bar exists).
		store
			.update_ref("refs/heads/foo/bar", a, None, ReflogIntent::Skip)
			.await
			.unwrap();

		// A batch creating a clean ref and then the conflicted one: the conflict rejects the batch, so
		// the clean ref is never committed.
		let ops = [
			crate::RefOp {
				name: "refs/heads/one".to_owned(),
				expected: None,
				new: Some(b),
				reflog: ReflogIntent::Skip,
			},
			crate::RefOp {
				name: "refs/heads/foo".to_owned(),
				expected: None,
				new: Some(b),
				reflog: ReflogIntent::Skip,
			},
		];
		let (name, _) = store
			.transact(&ops)
			.await
			.expect_err("a ref-name conflict must reject the batch");
		assert_eq!(name, "refs/heads/foo");
		assert_eq!(
			store.resolve("refs/heads/one").await.unwrap(),
			None,
			"the earlier clean op is not left committed"
		);

		let _ = std::fs::remove_dir_all(&tmp);
	}

	/// Aborting a transaction that locked a nested ref prunes the empty directory the lock created, so
	/// a later create of the parent ref is not blocked as a directory/file conflict.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn an_aborted_nested_lock_does_not_block_a_later_ref() {
		use gitana_file_store_local::LocalFileStore;

		let tmp = std::env::temp_dir().join(format!("gitana-lockprune-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(&tmp).unwrap();
		let files = LocalFileStore::from_dir(
			cap_std::fs::Dir::open_ambient_dir(&tmp, cap_std::ambient_authority()).unwrap(),
		);
		let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);
		let a = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"a");

		// Lock refs/heads/foo/bar (creating refs/heads/foo/) but abort in validation (a stale expected
		// value for a ref that is absent).
		let ops = [crate::RefOp {
			name: "refs/heads/foo/bar".to_owned(),
			expected: Some(a),
			new: Some(a),
			reflog: ReflogIntent::Skip,
		}];
		store
			.transact(&ops)
			.await
			.expect_err("a stale expected value aborts the transaction");

		// The abort pruned the empty refs/heads/foo/ the lock created, so creating refs/heads/foo now
		// succeeds instead of being rejected as a leftover-directory conflict.
		store
			.update_ref("refs/heads/foo", a, None, ReflogIntent::Skip)
			.await
			.expect("create refs/heads/foo after the aborted nested lock");
		assert_eq!(store.resolve("refs/heads/foo").await.unwrap(), Some(a));

		let _ = std::fs::remove_dir_all(&tmp);
	}
}
