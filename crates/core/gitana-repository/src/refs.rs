use std::marker::PhantomData;

use gitana_file_store::{FileStore, FileStoreError, PathLock};
use gitana_object::{HashAlgorithm, ObjectId};

use crate::{HeadLock, HeadResetPlan, HeadState, HeadTransaction, RefOp, RepositoryError};

/// The maximum symbolic-ref chain depth to follow (git's limit), a guard against a cycle.
const MAX_SYMREF_DEPTH: usize = 5;

/// Git's shared lock for rewriting and taking a stable snapshot of the packed ref namespace.
const PACKED_REFS: &str = "packed-refs";

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

struct HeldRefLocks {
	names: Vec<String>,
	locks: Vec<PathLock>,
}

struct PrefixSnapshot {
	ref_files: Vec<(String, Vec<u8>)>,
	reflog_files: Vec<(String, Vec<u8>)>,
	packed_refs: Vec<String>,
}

#[derive(PartialEq, Eq)]
struct SymbolicRefResolution {
	chain: Vec<String>,
	terminal: String,
}

#[cfg(not(target_arch = "wasm32"))]
struct OwnedRefOp<H: HashAlgorithm> {
	name: String,
	expected: Option<ObjectId<H>>,
	new: Option<ObjectId<H>>,
	reflog: OwnedReflogIntent,
}

#[cfg(not(target_arch = "wasm32"))]
enum OwnedReflogIntent {
	Log { committer: String, message: String },
	Skip,
}

#[cfg(not(target_arch = "wasm32"))]
impl OwnedReflogIntent {
	fn borrow(&self) -> ReflogIntent<'_> {
		match self {
			Self::Log { committer, message } => ReflogIntent::Log { committer, message },
			Self::Skip => ReflogIntent::Skip,
		}
	}
}

#[cfg(not(target_arch = "wasm32"))]
impl From<ReflogIntent<'_>> for OwnedReflogIntent {
	fn from(reflog: ReflogIntent<'_>) -> Self {
		match reflog {
			ReflogIntent::Log { committer, message } => Self::Log {
				committer: committer.to_owned(),
				message: message.to_owned(),
			},
			ReflogIntent::Skip => Self::Skip,
		}
	}
}

#[cfg(not(target_arch = "wasm32"))]
fn own_ref_ops<H: HashAlgorithm>(ops: &[RefOp<'_, H>]) -> Vec<OwnedRefOp<H>> {
	ops
		.iter()
		.map(|op| OwnedRefOp {
			name: op.name.clone(),
			expected: op.expected,
			new: op.new,
			reflog: OwnedReflogIntent::from(op.reflog),
		})
		.collect()
}

#[cfg(not(target_arch = "wasm32"))]
fn borrow_ref_ops<H: HashAlgorithm>(ops: &[OwnedRefOp<H>]) -> Vec<RefOp<'_, H>> {
	ops
		.iter()
		.map(|op| RefOp {
			name: op.name.clone(),
			expected: op.expected,
			new: op.new,
			reflog: op.reflog.borrow(),
		})
		.collect()
}

fn head_transaction_target(head_chain: &[String]) -> &str {
	head_chain
		.last()
		.map(String::as_str)
		.expect("a HEAD resolution always contains HEAD")
}

fn head_reset_ops<'a, H: HashAlgorithm>(
	head_chain: &[String],
	tip: Option<ObjectId<H>>,
	plan: &'a HeadResetPlan<H>,
) -> Vec<RefOp<'a, H>> {
	let mut ops = Vec::with_capacity(2);
	let terminal = head_transaction_target(head_chain);
	if let Some(tip) = tip
		&& terminal != "ORIG_HEAD"
	{
		ops.push(RefOp {
			name: "ORIG_HEAD".to_owned(),
			expected: plan.orig_head,
			new: Some(tip),
			reflog: ReflogIntent::Skip,
		});
	}
	let reflog = match (&plan.committer, &plan.message) {
		(Some(committer), Some(message)) => ReflogIntent::Log { committer, message },
		_ => ReflogIntent::Skip,
	};
	ops.push(RefOp {
		name: terminal.to_owned(),
		expected: tip,
		new: Some(plan.target),
		reflog,
	});
	ops
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
	/// [`Repository::refs`](crate::Repository::refs); a `None` leaves the store on the raw-local
	/// `config` fallback.
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
		for line in bytes.split(|byte| *byte == b'\n') {
			let line = line.strip_suffix(b"\r").unwrap_or(line);
			// Skip the header and `^<peeled>` lines.
			if line.starts_with(b"#") || line.starts_with(b"^") || line.is_empty() {
				continue;
			}
			if let Some((oid, refname)) = split_packed_ref(line)
				&& refname == name.as_bytes()
			{
				return Ok(Some(parse_oid(name, oid)?));
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
			for line in bytes.split(|byte| *byte == b'\n') {
				let line = line.strip_suffix(b"\r").unwrap_or(line);
				if line.starts_with(b"#") || line.starts_with(b"^") || line.is_empty() {
					continue;
				}
				if let Some((oid, name)) = split_packed_ref(line)
					&& name.starts_with(prefix.as_bytes())
				{
					let name = std::str::from_utf8(name).map_err(|_| {
						RepositoryError::InvalidRef(format!("packed ref under {prefix} is not UTF-8"))
					})?;
					refs.insert(name.to_owned(), parse_oid(name, oid)?);
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

	/// Resolve a direct or symbolic ref only when the initially requested loose name has the exact
	/// spelling supplied by the caller. Packed refs are already matched byte-for-byte. This prevents a
	/// case-insensitive filesystem from satisfying `refs/remotes/origin/HEAD` with an unrelated legal
	/// tracking ref such as `refs/remotes/origin/head`.
	pub async fn resolve_symbolic_exact(
		&self,
		name: &str,
	) -> Result<Option<ObjectId<H>>, RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			let name = name.to_owned();
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				store.resolve_symbolic_exact_inline(&name).await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self.resolve_symbolic_exact_inline(name).await
	}

	async fn resolve_symbolic_exact_inline(
		&self,
		name: &str,
	) -> Result<Option<ObjectId<H>>, RepositoryError> {
		let acquired = self
			.lock_all(&[name.to_owned(), PACKED_REFS.to_owned()])
			.await
			.map_err(|(_, error)| error)?;
		let result = async {
			if self.ref_name_resolves_through_alias(name).await? {
				return Ok(None);
			}
			self.follow_symref(name).await
		}
		.await;
		self.release_locks(acquired).await;
		result
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

	/// Compare-and-set a ref while following any symbolic chain at `name`, matching Git's default
	/// `update-ref` behavior. Every symbolic hop, the terminal ref, and `packed-refs` remain locked
	/// while the chain is revalidated and the terminal value is published, so a concurrent retarget
	/// cannot redirect or detach the update. The symbolic ref itself is preserved. When reflogging is
	/// enabled, every followed symbolic ref receives the same resolved old/new entry as Git; the
	/// terminal ref keeps the ordinary direct-update and split-HEAD logging rules.
	pub async fn update_ref_following_symbolic(
		&self,
		name: &str,
		new: ObjectId<H>,
		expected: Option<ObjectId<H>>,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			let name = name.to_owned();
			let reflog = OwnedReflogIntent::from(reflog);
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				store
					.update_ref_following_symbolic_inline(&name, new, expected, reflog.borrow())
					.await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self
			.update_ref_following_symbolic_inline(name, new, expected, reflog)
			.await
	}

	async fn update_ref_following_symbolic_inline(
		&self,
		name: &str,
		new: ObjectId<H>,
		expected: Option<ObjectId<H>>,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		let policy = self.reflog_policy().await?;
		let planned = self.symbolic_ref_resolution(name).await?;
		let head_target = self.read_symbolic("HEAD").await?;
		let cascade = planned.terminal.starts_with("refs/heads/")
			&& head_target.as_deref() == Some(planned.terminal.as_str());
		let mut lock_names = planned.chain.clone();
		if cascade {
			lock_names.push("HEAD".to_owned());
		}
		if planned.terminal.starts_with("refs/") {
			lock_names.push(PACKED_REFS.to_owned());
		}
		let acquired = self
			.lock_all(&lock_names)
			.await
			.map_err(|(_, error)| error)?;
		let result = async {
			let observed = self.symbolic_ref_resolution(name).await?;
			if observed.chain != planned.chain || observed.terminal != planned.terminal {
				return Err(RepositoryError::RefMoved {
					name: name.to_owned(),
				});
			}
			let symbolic_hops = observed
				.chain
				.iter()
				.take(observed.chain.len().saturating_sub(1))
				.cloned()
				.collect::<Vec<_>>();
			let op = RefOp {
				name: observed.terminal,
				expected,
				new: Some(new),
				reflog,
			};
			let cascades = self
				.confirm_cascades(std::slice::from_ref(&op), vec![cascade])
				.await
				.map_err(|(_, error)| error)?;
			let olds = self
				.validate_locked(std::slice::from_ref(&op), &cascades, policy)
				.await
				.map_err(|(_, error)| error)?;
			// Git logs the requested symbolic name and every intermediate hop, even for a no-op;
			// the terminal direct ref retains the ordinary rule that suppresses a no-op entry. When
			// `HEAD` is itself a followed hop, the terminal branch's split-HEAD cascade owns that one
			// log entry, so exclude it here rather than appending it twice.
			let mut logged_hops = Vec::new();
			if matches!(reflog, ReflogIntent::Log { .. }) {
				for hop in symbolic_hops {
					if cascades[0] && hop == "HEAD" {
						continue;
					}
					if self.should_log(&hop, policy).await? {
						if self.path_write_blocked(&format!("logs/{hop}")).await? {
							return Err(RepositoryError::InvalidRef(format!(
								"{name}: reflog path {hop} blocked by an existing file or directory"
							)));
						}
						logged_hops.push(hop);
					}
				}
			}
			if let ReflogIntent::Log { committer, message } = reflog {
				for hop in logged_hops {
					self
						.append_reflog(&hop, olds[0], Some(new), committer, message)
						.await?;
				}
			}
			self
				.commit_validated(std::slice::from_ref(&op), &olds, &cascades, policy)
				.await
				.map_err(|(_, error)| error)
		}
		.await;
		self.release_locks(acquired).await;
		result
	}

	async fn symbolic_ref_resolution(
		&self,
		name: &str,
	) -> Result<SymbolicRefResolution, RepositoryError> {
		let mut current = name.to_owned();
		let mut chain = Vec::new();
		for _ in 0..MAX_SYMREF_DEPTH {
			if chain.contains(&current) {
				return Err(RepositoryError::InvalidRef(format!(
					"{name}: symbolic ref cycle"
				)));
			}
			chain.push(current.clone());
			match self.files.read_path(&current).await {
				Ok(bytes) => {
					let text = std::str::from_utf8(&bytes)
						.map_err(|_| RepositoryError::InvalidRef(current.clone()))?
						.trim();
					if let Some(target) = text.strip_prefix("ref:") {
						let target = target.trim();
						if !is_valid_refname(target) {
							return Err(RepositoryError::InvalidRef(format!(
								"{current}: invalid symbolic target {target}"
							)));
						}
						current = target.to_owned();
					} else {
						return Ok(SymbolicRefResolution {
							chain,
							terminal: current.clone(),
						});
					}
				}
				Err(FileStoreError::NotFound) => {
					self.resolve_packed(&current).await?;
					return Ok(SymbolicRefResolution {
						chain,
						terminal: current.clone(),
					});
				}
				Err(other) => return Err(other.into()),
			}
		}
		Err(RepositoryError::InvalidRef(format!(
			"{name}: symbolic ref chain is too deep"
		)))
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
	/// Every op's ref (and `HEAD`, for a split-HEAD reflog cascade) is locked via `<ref>.lock`.
	/// Transactions touching `refs/` also hold `packed-refs.lock`, making the packed namespace used by
	/// validation stable through publication and excluding concurrent packed-ref rewrites. Locks are
	/// acquired in a fixed sorted order so concurrent transactions cannot deadlock; every precondition
	/// is validated; then each op's reflog is written and its ref committed. Any failure applies
	/// nothing and returns the offending ref name and error.
	///
	/// Because an op writes its reflog *before* its ref while holding the lock, a reflog-write failure
	/// or a lost CAS race leaves the ref untouched — the atomicity a raw ref move lacked.
	/// [`update_ref`](Self::update_ref) and [`delete_ref`](Self::delete_ref) are one-op wrappers; a
	/// caller wanting all-or-nothing across several refs (a `--atomic` push) passes them together.
	///
	/// On native targets, the complete transaction runs in an owned task. Dropping the calling future
	/// only stops waiting for its result: lock acquisition, validation, publication, lock release, and
	/// directory pruning continue to completion. Wasm file-store operations execute synchronously when
	/// polled, so the transaction runs inline there without requiring a Tokio runtime.
	pub async fn transact(&self, ops: &[RefOp<'_, H>]) -> Result<(), (String, RepositoryError)> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			let ops = own_ref_ops(ops);
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				let ops = borrow_ref_ops(&ops);
				store.transact_inline(&ops).await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err((
					String::new(),
					RepositoryError::RetainedTask(error.to_string()),
				)),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self.transact_inline(ops).await
	}

	async fn transact_inline(&self, ops: &[RefOp<'_, H>]) -> Result<(), (String, RepositoryError)> {
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
		if ops.iter().any(|op| op.name.starts_with("refs/")) {
			lock_names.push(PACKED_REFS.to_owned());
		}
		let packed_owner = ops
			.iter()
			.find(|op| op.name.starts_with("refs/"))
			.map(|op| op.name.clone());
		let acquired = self.lock_all(&lock_names).await.map_err(|(name, error)| {
			let owner = if name == PACKED_REFS {
				packed_owner.clone().unwrap_or(name)
			} else {
				name
			};
			(owner, error)
		})?;

		// Confirm the cascade under the acquired locks (catching a `HEAD` retarget in the
		// pre-lock→lock window), validate, then commit. On native this entire sequence lives in the
		// retained worker, so the path-lock guards cannot be dropped while a backend write continues.
		let outcome = async {
			let cascades = self.confirm_cascades(ops, cascades).await?;
			let olds = self.validate_locked(ops, &cascades, policy).await?;
			self.commit_validated(ops, &olds, &cascades, policy).await
		}
		.await;

		let HeldRefLocks { names, locks } = acquired;
		drop(locks);
		for name in &names {
			self.prune_empty_dirs(name).await;
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

	/// Validate every op while the full lock set is held, returning each observed old value.
	async fn validate_locked(
		&self,
		ops: &[RefOp<'_, H>],
		cascades: &[bool],
		policy: ReflogPolicy,
	) -> Result<Vec<Option<ObjectId<H>>>, (String, RepositoryError)> {
		// Validate all preconditions before mutating anything — so the common rejections (a stale
		// `expected`, deleting a missing ref) apply nothing, even in a multi-op transaction.
		let packed_owner = ops.iter().find(|op| op.name.starts_with("refs/"));
		let packed_refs = if ops
			.iter()
			.any(|op| op.new.is_some() && op.name.starts_with("refs/"))
		{
			self.read_opt("packed-refs").await.map_err(|error| {
				(
					packed_owner
						.expect("a packed snapshot requires an operation under refs/")
						.name
						.clone(),
					error,
				)
			})?
		} else {
			None
		};
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
					.ref_path_write_blocked(&op.name, packed_refs.as_deref())
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

		Ok(olds)
	}

	/// Commit prevalidated ops while both the ref locks and any caller guard remain held.
	async fn commit_validated(
		&self,
		ops: &[RefOp<'_, H>],
		olds: &[Option<ObjectId<H>>],
		cascades: &[bool],
		policy: ReflogPolicy,
	) -> Result<(), (String, RepositoryError)> {
		// Validation preflighted every directory/file conflict, so a commit can now fail only on
		// catastrophic I/O — nothing else moves a ref and then reports failure.
		for ((op, &old), &cascade) in ops.iter().zip(olds).zip(cascades) {
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
				if op.name.starts_with("refs/") {
					self.remove_from_packed_locked(&op.name).await?;
				}
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

	/// Whether writing a ref at `target` would conflict with either the loose storage namespace or a
	/// strict ancestor/descendant in `packed-refs`. An exact packed ref is allowed: the loose write
	/// replaces it by shadowing its packed value, as Git does.
	async fn ref_path_write_blocked(
		&self,
		target: &str,
		packed_refs: Option<&[u8]>,
	) -> Result<bool, RepositoryError> {
		Ok(
			self.path_write_blocked(target).await?
				|| packed_refs.is_some_and(|packed| packed_ref_path_conflict(packed, target)),
		)
	}

	/// Whether writing a value at `target` would hit a loose directory/file conflict: `target` is a
	/// directory (a leftover from a nested ref/reflog, e.g. `refs/heads/foo` when `refs/heads/foo/bar`
	/// exists, an implied directory on a flat store, or an empty dir a delete left behind), or a strict
	/// ancestor is a *file* (blocking the
	/// intermediate directory, e.g. a stray `logs/refs/heads/foo` file under `logs/refs/heads/foo/bar`).
	///
	/// A transaction preflights this for a move's ref path and its reflog path, so a validated commit
	/// cannot fail on such a conflict. `read_path` catches a file ancestor — it reads back `Ok` only
	/// for a file (a directory or absent path errors, with a backend-varying kind, so we key on `Ok`).
	/// Ancestors are checked before `is_dir`, because native metadata on the target can report
	/// `NotADirectory` when an ancestor is the blocker. `is_dir` then catches the target-directory case
	/// (including empty directories).
	async fn path_write_blocked(&self, target: &str) -> Result<bool, RepositoryError> {
		for (index, _) in target.match_indices('/') {
			if self.files.read_path(&target[..index]).await.is_ok() {
				return Ok(true);
			}
		}
		self.files.is_dir(target).await.map_err(Into::into)
	}

	/// Acquire every `<name>.lock` in `names`, sorted and deduped so concurrent transactions take
	/// shared locks in the same order (deadlock-free). Ordinary ref locks sort first and
	/// `packed-refs.lock` sorts last, matching stock Git's update-ref protocol. On the first contended
	/// lock, releases those already taken and reports it.
	async fn lock_all(&self, names: &[String]) -> Result<HeldRefLocks, (String, RepositoryError)> {
		let mut sorted: Vec<String> = names.to_vec();
		sorted.sort_by(
			|left, right| match (left == PACKED_REFS, right == PACKED_REFS) {
				(false, true) => std::cmp::Ordering::Less,
				(true, false) => std::cmp::Ordering::Greater,
				_ => left.cmp(right),
			},
		);
		sorted.dedup();
		let mut acquired = HeldRefLocks {
			names: Vec::with_capacity(sorted.len()),
			locks: Vec::with_capacity(sorted.len()),
		};
		for name in sorted {
			let lock = match self.lock_ref(&name).await {
				Ok(lock) => lock,
				Err(error) => {
					let HeldRefLocks { mut names, locks } = acquired;
					drop(locks);
					names.push(name.clone());
					for acquired_name in &names {
						self.prune_empty_dirs(acquired_name).await;
					}
					return Err((name, error));
				}
			};
			acquired.names.push(name);
			acquired.locks.push(lock);
		}
		Ok(acquired)
	}

	async fn release_locks(&self, acquired: HeldRefLocks) {
		let HeldRefLocks { names, locks } = acquired;
		drop(locks);
		for name in &names {
			self.prune_empty_dirs(name).await;
		}
	}

	/// Take `<name>.lock` (git's ref lock), retrying briefly on contention before giving up with
	/// [`RepositoryError::RefLocked`].
	pub(crate) async fn lock_ref(&self, name: &str) -> Result<PathLock, RepositoryError> {
		let path = format!("{name}.lock");
		for attempt in 0..LOCK_ATTEMPTS {
			match self.files.try_lock_path(&path).await? {
				Some(lock) => return Ok(lock),
				None => {
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

	/// Best-effort removal of `path`'s now-empty ancestor directories, from the innermost up, stopping
	/// at the first that is not an empty directory (or on any error / a backend without directories).
	///
	/// **Namespace anchors are never removed:** a directory of two or fewer path components — `refs`,
	/// `refs/heads`, `logs/refs`, … — is preserved, matching git, which skips the first two components
	/// when pruning a ref's empty parents. `refs/` in particular must survive or the repository stops
	/// being recognized (`is_git_dir` requires it). Deeper empties (e.g. `refs/heads/foo` left when
	/// `refs/heads/foo/bar` is deleted) are pruned so the parent name is free to become a ref.
	pub(crate) async fn prune_empty_dirs(&self, path: &str) {
		let mut current = path;
		while let Some(index) = current.rfind('/') {
			let parent = &current[..index];
			if parent.matches('/').count() < 2 {
				break;
			}
			if self.files.remove_dir(parent).await.is_err() {
				break;
			}
			current = parent;
		}
	}

	/// Delete every ref under `prefix` — loose (direct *or* symbolic), its reflog, and any packed
	/// entry alike. Used to drop a remote's whole `refs/remotes/<name>/` tree, which
	/// [`Self::delete_ref`] cannot: it resolves and value-checks a single, non-symbolic ref.
	pub async fn remove_prefix(&self, prefix: &str) -> Result<(), RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			let prefix = prefix.to_owned();
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				store.remove_prefix_inline(&prefix).await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self.remove_prefix_inline(prefix).await
	}

	async fn remove_prefix_inline(&self, prefix: &str) -> Result<(), RepositoryError> {
		for _ in 0..LOCK_ATTEMPTS {
			let snapshot = self.prefix_snapshot(prefix).await?;
			let lock_names = remove_prefix_lock_names(&snapshot);
			let acquired = self
				.lock_all(&lock_names)
				.await
				.map_err(|(_, error)| error)?;
			let snapshot = match self.prefix_snapshot(prefix).await {
				Ok(snapshot) => snapshot,
				Err(error) => {
					self.release_locks(acquired).await;
					return Err(error);
				}
			};
			if !locks_cover(&acquired, &remove_prefix_lock_names(&snapshot)) {
				self.release_locks(acquired).await;
				continue;
			}

			// Delete lock-free while the corresponding ref locks and `packed-refs.lock` remain held. A
			// symbolic loose ref has no oid, so it is intentionally removed as an ordinary file too.
			let result = async {
				for (path, _) in snapshot.ref_files.iter().chain(&snapshot.reflog_files) {
					self.files.delete_path_unlocked(path).await?;
				}
				self
					.remove_packed_matching_locked(|name| name.starts_with(prefix))
					.await
			}
			.await;
			self.release_locks(acquired).await;
			return result;
		}

		Err(RepositoryError::RefMoved {
			name: prefix.to_owned(),
		})
	}

	/// Move every ref under `old` to `new` — loose (direct *or* symbolic, rewriting a symbolic target
	/// that points back under `old`), its reflog, and any packed entry alike. Used to rename a
	/// remote's whole `refs/remotes/<old>/` tree to `refs/remotes/<new>/`.
	pub async fn rename_prefix(&self, old: &str, new: &str) -> Result<(), RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			let old = old.to_owned();
			let new = new.to_owned();
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				store.rename_prefix_inline(&old, &new).await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self.rename_prefix_inline(old, new).await
	}

	async fn rename_prefix_inline(&self, old: &str, new: &str) -> Result<(), RepositoryError> {
		for _ in 0..LOCK_ATTEMPTS {
			let snapshot = self.prefix_snapshot(old).await?;
			let lock_names = rename_prefix_lock_names(&snapshot, old, new);
			let acquired = self
				.lock_all(&lock_names)
				.await
				.map_err(|(_, error)| error)?;
			let snapshot = match self.prefix_snapshot(old).await {
				Ok(snapshot) => snapshot,
				Err(error) => {
					self.release_locks(acquired).await;
					return Err(error);
				}
			};
			if !locks_cover(&acquired, &rename_prefix_lock_names(&snapshot, old, new)) {
				self.release_locks(acquired).await;
				continue;
			}

			let result = self.rename_prefix_locked(old, new, &snapshot).await;
			self.release_locks(acquired).await;
			return result;
		}

		Err(RepositoryError::RefMoved {
			name: old.to_owned(),
		})
	}

	async fn rename_prefix_locked(
		&self,
		old: &str,
		new: &str,
		snapshot: &PrefixSnapshot,
	) -> Result<(), RepositoryError> {
		// Loose ref files, rewriting a symbolic `ref:` target that itself points under `old`. Keep the
		// paths we write — they are the destination's *authoritative* loose refs, so the stale-shadow
		// sweep below must not delete them.
		let loose_targets = self
			.move_locked_files(&snapshot.ref_files, old, new, |bytes| {
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
			.move_locked_files(
				&snapshot.reflog_files,
				&format!("logs/{old}"),
				&format!("logs/{new}"),
				<[u8]>::to_vec,
			)
			.await?;
		// Packed entries: rewrite each `<old>…` ref name to `<new>…`, returning the renamed destinations.
		let renamed_dests = self.rename_packed_prefix_locked(old, new).await?;
		// A stale *loose* ref already sitting at a renamed packed ref's destination would shadow it,
		// leaving the tracking branch on the old commit. Git's rename overwrites the destination, so
		// drop any such loose ref — except one we just wrote by moving the source remote's own refs.
		for dest in &renamed_dests {
			if loose_targets.contains(dest) {
				continue;
			}
			self.files.delete_path_unlocked(dest).await?;
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
	async fn move_locked_files(
		&self,
		files: &[(String, Vec<u8>)],
		old: &str,
		new: &str,
		rewrite: impl Fn(&[u8]) -> Vec<u8>,
	) -> Result<Vec<String>, RepositoryError> {
		let moves: Vec<(String, Vec<u8>)> = files
			.iter()
			.map(|(path, bytes)| (format!("{new}{}", &path[old.len()..]), rewrite(bytes)))
			.collect();
		let targets: Vec<String> = moves.iter().map(|(target, _)| target.clone()).collect();
		// Validate every destination before publishing any of them. In particular, a surviving child
		// makes its parent a directory (`.../main/foo` blocks `.../main`); attempting the atomic replace
		// in that state would otherwise create a temporary sibling before the final rename fails.
		for target in &targets {
			if self.path_write_blocked(target).await? {
				return Err(RepositoryError::InvalidRef(format!(
					"{target}: blocked by an existing directory or file"
				)));
			}
		}
		for (target, bytes) in &moves {
			self.files.write_path_replace(target, bytes).await?;
		}
		let target_set: std::collections::HashSet<&str> = targets.iter().map(String::as_str).collect();
		for (path, _) in files {
			if target_set.contains(path.as_str()) {
				continue;
			}
			self.files.delete_path_unlocked(path).await?;
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
	async fn rename_packed_prefix_locked(
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
		self
			.files
			.write_path_replace(PACKED_REFS, out.as_bytes())
			.await?;
		Ok(renamed_dests)
	}

	async fn prefix_snapshot(&self, prefix: &str) -> Result<PrefixSnapshot, RepositoryError> {
		let ref_files = self.collect_files_under(prefix).await?;
		let reflog_files = self.collect_files_under(&format!("logs/{prefix}")).await?;
		let packed_refs = self
			.read_packed_ref_names()
			.await?
			.into_iter()
			.filter(|name| name.starts_with(prefix))
			.collect();
		Ok(PrefixSnapshot {
			ref_files,
			reflog_files,
			packed_refs,
		})
	}

	/// Buffer every ordinary file under `prefix`, descending through physical or flat-store logical
	/// directories. Lock artifacts are protocol state, not refs or reflogs, and are never included.
	async fn collect_files_under(
		&self,
		prefix: &str,
	) -> Result<Vec<(String, Vec<u8>)>, RepositoryError> {
		let mut stack = vec![prefix.to_owned()];
		let mut files = Vec::new();
		while let Some(dir) = stack.pop() {
			for path in self.files.list_prefix(&dir).await? {
				if path.ends_with(".lock") {
					continue;
				}
				if self.files.is_dir(&path).await? {
					stack.push(format!("{path}/"));
					continue;
				}
				match self.files.read_path(&path).await {
					Ok(bytes) => files.push((path, bytes)),
					Err(FileStoreError::NotFound) => {}
					Err(other) => return Err(other.into()),
				}
			}
		}
		Ok(files)
	}

	async fn read_packed_ref_names(&self) -> Result<Vec<String>, RepositoryError> {
		let Some(bytes) = self.read_opt(PACKED_REFS).await? else {
			return Ok(Vec::new());
		};
		let text = std::str::from_utf8(&bytes)
			.map_err(|_| RepositoryError::InvalidRef("packed-refs not UTF-8".to_owned()))?;
		Ok(
			text
				.lines()
				.filter(|line| !line.starts_with('#') && !line.starts_with('^') && !line.is_empty())
				.filter_map(|line| line.split_once(' ').map(|(_, name)| name.to_owned()))
				.collect(),
		)
	}

	/// Rewrite `packed-refs` without `name` (and its `^<peeled>` continuation line) while the caller
	/// holds `packed-refs.lock`. A no-op if there is no packed-refs file or the ref is not packed.
	async fn remove_from_packed_locked(&self, name: &str) -> Result<(), RepositoryError> {
		self
			.remove_packed_matching_locked(|refname| refname == name)
			.await
	}

	/// Rewrite `packed-refs` without the entries `drop` selects (and their `^<peeled>` continuations)
	/// while the caller holds `packed-refs.lock`. A no-op if there is no packed-refs file or nothing
	/// matches.
	async fn remove_packed_matching_locked(
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
			self
				.files
				.write_path_replace(PACKED_REFS, out.as_bytes())
				.await?;
		}
		Ok(())
	}

	/// Acquire this worktree's `HEAD.lock` as a typed checkout capability.
	///
	/// The returned [`HeadLock`] owns a handle to this exact store and can only be consumed by
	/// publishing `HEAD`. Checkout code holds it from before it reads the merge base through the
	/// working-tree mutation, so the branch cannot move under an in-flight checkout.
	pub async fn lock_head(&self) -> Result<HeadLock<F::Shared, H>, RepositoryError> {
		let files = self.files.shared_handle();
		let effective = self.effective.cloned();
		let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
		let lock = store.lock_ref("HEAD").await?;
		Ok(HeadLock::new(files, effective, lock))
	}

	/// Lock `HEAD`, every symbolic hop, the terminal ref, and `ORIG_HEAD` for reset-style history
	/// integration.
	///
	/// Holding the returned capability prevents a concurrent checkout from retargeting `HEAD` and
	/// prevents the starting branch from moving during a worktree merge. The complete set is sorted
	/// and deduplicated before acquisition so transactions use the same ordering as multi-ref writes,
	/// including when a symbolic chain itself terminates at `ORIG_HEAD`.
	pub async fn lock_head_transaction(
		&self,
	) -> Result<HeadTransaction<F::Shared, H>, RepositoryError> {
		let files = self.files.shared_handle();
		let effective = self.effective.cloned();
		let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
		let state = store.read_head().await?;
		let planned = store.symbolic_ref_resolution("HEAD").await?;
		let mut names = planned.chain.clone();
		names.push("ORIG_HEAD".to_owned());
		if planned.terminal.starts_with("refs/") {
			names.push(PACKED_REFS.to_owned());
		}
		let acquired = store.lock_all(&names).await.map_err(|(_, error)| error)?;
		let observed_state = match store.read_head().await {
			Ok(observed) => observed,
			Err(error) => {
				store.release_locks(acquired).await;
				return Err(error);
			}
		};
		let observed = match store.symbolic_ref_resolution("HEAD").await {
			Ok(observed) => observed,
			Err(error) => {
				store.release_locks(acquired).await;
				return Err(error);
			}
		};
		if observed_state != state
			|| observed.chain != planned.chain
			|| observed.terminal != planned.terminal
		{
			store.release_locks(acquired).await;
			return Err(RepositoryError::RefMoved {
				name: "HEAD".to_owned(),
			});
		}
		let tip = match &state {
			HeadState::Symbolic(_) => match store.resolve(&planned.terminal).await {
				Ok(tip) => tip,
				Err(error) => {
					store.release_locks(acquired).await;
					return Err(error);
				}
			},
			HeadState::Detached(oid) => Some(*oid),
		};
		let HeldRefLocks { names, locks } = acquired;
		Ok(HeadTransaction {
			files,
			effective,
			locks,
			lock_names: names,
			state,
			head_chain: planned.chain,
			tip,
			prepared: None,
		})
	}

	pub(crate) async fn prepare_head_reset(
		&self,
		state: &HeadState<H>,
		head_chain: &[String],
		tip: Option<ObjectId<H>>,
		target: ObjectId<H>,
		reflog: ReflogIntent<'_>,
	) -> Result<HeadResetPlan<H>, RepositoryError> {
		self.validate_head_snapshot(state, head_chain, tip).await?;
		let orig_head = if tip.is_some() && head_transaction_target(head_chain) != "ORIG_HEAD" {
			self.resolve("ORIG_HEAD").await?
		} else {
			None
		};
		let (committer, message) = match reflog {
			ReflogIntent::Log { committer, message } => {
				(Some(committer.to_owned()), Some(message.to_owned()))
			}
			ReflogIntent::Skip => (None, None),
		};
		let plan = HeadResetPlan {
			target,
			orig_head,
			committer,
			message,
		};
		let ops = head_reset_ops(head_chain, tip, &plan);
		let cascades = vec![false; ops.len()];
		let policy = self.reflog_policy().await?;
		self
			.validate_locked(&ops, &cascades, policy)
			.await
			.map_err(|(_, error)| error)?;
		self
			.head_reset_symbolic_reflogs(state, head_chain, &plan, policy)
			.await?;
		Ok(plan)
	}

	pub(crate) async fn commit_head_reset(
		&self,
		state: &HeadState<H>,
		head_chain: &[String],
		tip: Option<ObjectId<H>>,
		plan: &HeadResetPlan<H>,
	) -> Result<(), RepositoryError> {
		self.validate_head_snapshot(state, head_chain, tip).await?;
		let ops = head_reset_ops(head_chain, tip, plan);
		let cascades = vec![false; ops.len()];
		let policy = self.reflog_policy().await?;
		let olds = self
			.validate_locked(&ops, &cascades, policy)
			.await
			.map_err(|(_, error)| error)?;
		let symbolic_reflogs = self
			.head_reset_symbolic_reflogs(state, head_chain, plan, policy)
			.await?;
		if let (Some(committer), Some(message)) = (&plan.committer, &plan.message) {
			for name in symbolic_reflogs {
				self
					.append_reflog(&name, tip, Some(plan.target), committer, message)
					.await?;
			}
		}
		self
			.commit_validated(&ops, &olds, &cascades, policy)
			.await
			.map_err(|(_, error)| error)
	}

	pub(crate) async fn commit_head_orig(
		&self,
		state: &HeadState<H>,
		head_chain: &[String],
		tip: ObjectId<H>,
	) -> Result<(), RepositoryError> {
		self
			.validate_head_snapshot(state, head_chain, Some(tip))
			.await?;
		if head_transaction_target(head_chain) == "ORIG_HEAD" {
			return Ok(());
		}
		let op = RefOp {
			name: "ORIG_HEAD".to_owned(),
			expected: self.resolve("ORIG_HEAD").await?,
			new: Some(tip),
			reflog: ReflogIntent::Skip,
		};
		let cascades = [false];
		let policy = self.reflog_policy().await?;
		let olds = self
			.validate_locked(std::slice::from_ref(&op), &cascades, policy)
			.await
			.map_err(|(_, error)| error)?;
		self
			.commit_validated(std::slice::from_ref(&op), &olds, &cascades, policy)
			.await
			.map_err(|(_, error)| error)
	}

	async fn validate_head_snapshot(
		&self,
		state: &HeadState<H>,
		head_chain: &[String],
		tip: Option<ObjectId<H>>,
	) -> Result<(), RepositoryError> {
		if self.read_head().await? != *state {
			return Err(RepositoryError::RefMoved {
				name: "HEAD".to_owned(),
			});
		}
		let observed = self.symbolic_ref_resolution("HEAD").await?;
		if observed.chain != head_chain || observed.terminal != head_transaction_target(head_chain) {
			return Err(RepositoryError::RefMoved {
				name: "HEAD".to_owned(),
			});
		}
		let observed_tip = match state {
			HeadState::Symbolic(_) => self.resolve(&observed.terminal).await?,
			HeadState::Detached(oid) => Some(*oid),
		};
		if observed_tip != tip {
			return Err(RepositoryError::RefMoved {
				name: observed.terminal,
			});
		}
		Ok(())
	}

	async fn head_reset_symbolic_reflogs(
		&self,
		state: &HeadState<H>,
		head_chain: &[String],
		plan: &HeadResetPlan<H>,
		policy: ReflogPolicy,
	) -> Result<Vec<String>, RepositoryError> {
		if !matches!(state, HeadState::Symbolic(_))
			|| plan.committer.is_none()
			|| plan.message.is_none()
		{
			return Ok(Vec::new());
		}

		let mut logged = Vec::new();
		for name in head_chain.iter().take(head_chain.len().saturating_sub(1)) {
			if self.should_log(name, policy).await? {
				if self.path_write_blocked(&format!("logs/{name}")).await? {
					return Err(RepositoryError::InvalidRef(format!(
						"HEAD: reflog path {name} blocked by an existing file or directory"
					)));
				}
				logged.push(name.clone());
			}
		}
		Ok(logged)
	}

	pub(crate) async fn prune_lock_parents(&self, name: &str) {
		self.prune_empty_dirs(name).await;
	}

	/// Publish a checkout while the caller's `head_lock` (this worktree's `HEAD.lock`) is held, consuming
	/// it: optionally create `branch` at `create.0` (git's `switch -c`), then point `HEAD` at `branch`.
	///
	/// Backs [`HeadLock::finish_checkout`](crate::HeadLock::finish_checkout). On native the whole
	/// publication runs in an owned task that OWNS `head_lock` (and, for the create, the branch's own
	/// lock), so a cancelled checkout cannot release `HEAD.lock` between the create and the `HEAD` write.
	pub(crate) async fn commit_checkout(
		&self,
		head_lock: PathLock,
		branch: &str,
		create: Option<(ObjectId<H>, ReflogIntent<'_>)>,
		checkout_reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			let branch = branch.to_owned();
			let create = create.map(|(target, reflog)| (target, OwnedReflogIntent::from(reflog)));
			let checkout_reflog = OwnedReflogIntent::from(checkout_reflog);
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				let create = create
					.as_ref()
					.map(|(target, reflog)| (*target, reflog.borrow()));
				store
					.commit_checkout_inline(head_lock, &branch, create, checkout_reflog.borrow())
					.await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self
			.commit_checkout_inline(head_lock, branch, create, checkout_reflog)
			.await
	}

	/// Validate a detached `HEAD` update while the caller retains `HEAD.lock`. No repository state is
	/// changed: the returned reflog image and target can be committed after the worktree checkout.
	pub(crate) async fn prepare_detached_checkout(
		&self,
		target: ObjectId<H>,
		reflog: ReflogIntent<'_>,
	) -> Result<Option<Vec<u8>>, RepositoryError> {
		if self.ref_path_write_blocked("HEAD", None).await? {
			return Err(RepositoryError::InvalidRef(
				"HEAD: blocked by an existing directory or file".to_owned(),
			));
		}
		let old = self.follow_symref("HEAD").await?;
		if let ReflogIntent::Log { committer, message } = reflog
			&& self.should_log("HEAD", self.reflog_policy().await?).await?
		{
			if self.path_write_blocked("logs/HEAD").await? {
				return Err(RepositoryError::InvalidRef(
					"HEAD: reflog path blocked by an existing file or directory".to_owned(),
				));
			}
			let mut content = match self.files.read_path("logs/HEAD").await {
				Ok(bytes) => bytes,
				Err(FileStoreError::NotFound) => Vec::new(),
				Err(other) => return Err(other.into()),
			};
			content.extend_from_slice(&reflog_line(old, Some(target), committer, message));
			return Ok(Some(content));
		}
		Ok(None)
	}

	/// Publish a prepared detached `HEAD` while consuming the already-held checkout lock.
	pub(crate) async fn commit_prepared_detached_checkout(
		&self,
		head_lock: PathLock,
		target: ObjectId<H>,
		reflog_content: Option<Vec<u8>>,
	) -> Result<(), RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				store
					.commit_prepared_detached_checkout_inline(head_lock, target, reflog_content)
					.await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self
			.commit_prepared_detached_checkout_inline(head_lock, target, reflog_content)
			.await
	}

	async fn commit_prepared_detached_checkout_inline(
		&self,
		head_lock: PathLock,
		target: ObjectId<H>,
		reflog_content: Option<Vec<u8>>,
	) -> Result<(), RepositoryError> {
		let result = async {
			if let Some(content) = reflog_content {
				self.files.write_path_replace("logs/HEAD", &content).await?;
			}
			let bytes = HeadState::<H>::Detached(target).render();
			self
				.files
				.write_path_replace("HEAD", bytes.as_bytes())
				.await?;
			Ok(())
		}
		.await;
		drop(head_lock);
		self.prune_empty_dirs("HEAD").await;
		result
	}

	/// The body of [`commit_checkout`](Self::commit_checkout), run holding `head_lock`.
	async fn commit_checkout_inline(
		&self,
		head_lock: PathLock,
		branch: &str,
		create: Option<(ObjectId<H>, ReflogIntent<'_>)>,
		checkout_reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		// git's order: create the branch first, then point `HEAD` at it, so the (possibly just-created)
		// branch resolves for `HEAD`'s reflog `old`/`new` (git writes `<tip> <tip> checkout: …`, not a
		// zero old, and emits the entry rather than skipping an unresolved target).
		if let Some((target, reflog)) = create {
			self.create_under_head_lock(branch, target, reflog).await?;
		}
		let result = self
			.set_symbolic_locked("HEAD", branch, checkout_reflog)
			.await;
		drop(head_lock);
		self.prune_empty_dirs("HEAD").await;
		result
	}

	/// Create `branch` at `target` while the checkout's `HEAD.lock` is held (not re-locked): a one-op
	/// transaction that locks the branch, validates its absence, and commits. When `HEAD` points at
	/// `branch` (an unborn branch being born) the create cascades into `logs/HEAD`; because the checkout
	/// already holds `HEAD.lock`, that split-HEAD reflog is written under it rather than by re-acquiring
	/// it — the deadlock a bare [`transact`](Self::transact) would hit.
	async fn create_under_head_lock(
		&self,
		branch: &str,
		target: ObjectId<H>,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		let policy = self.reflog_policy().await?;
		let op = RefOp {
			name: branch.to_owned(),
			expected: None,
			new: Some(target),
			reflog,
		};
		let ops = std::slice::from_ref(&op);
		let acquired = self
			.lock_all(&[PACKED_REFS.to_owned(), branch.to_owned()])
			.await
			.map_err(|(_, error)| error)?;
		let result = async {
			// The cascade decision uses `HEAD` — stable, since the checkout holds `HEAD.lock`.
			let head_target = self.read_symbolic("HEAD").await?;
			let cascades =
				vec![branch.starts_with("refs/heads/") && head_target.as_deref() == Some(branch)];
			let cascades = self
				.confirm_cascades(ops, cascades)
				.await
				.map_err(|(_, error)| error)?;
			let olds = self
				.validate_locked(ops, &cascades, policy)
				.await
				.map_err(|(_, error)| error)?;
			self
				.commit_validated(ops, &olds, &cascades, policy)
				.await
				.map_err(|(_, error)| error)
		}
		.await;
		let HeldRefLocks { names, locks } = acquired;
		drop(locks);
		for name in &names {
			self.prune_empty_dirs(name).await;
		}
		result
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
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			let name = name.to_owned();
			let target = target.to_owned();
			let reflog = OwnedReflogIntent::from(reflog);
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				store
					.set_symbolic_inline(&name, &target, reflog.borrow())
					.await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self.set_symbolic_inline(name, target, reflog).await
	}

	/// Publish a non-essential direct or symbolic ref when its namespace can safely represent it.
	///
	/// Unlike the ordinary ref-moving APIs, a directory/file conflict, malformed loose occupant, or
	/// filesystem-equivalent differently spelled occupant at the exact name is an expected omission and
	/// returns `false` without changing it. Other failures remain errors. The publication is unlogged and
	/// holds the ref and packed-ref locks across its final validation and write, making the decision atomic
	/// with other ref operations.
	pub async fn publish_optional_ref(
		&self,
		name: &str,
		state: HeadState<H>,
	) -> Result<bool, RepositoryError> {
		#[cfg(not(target_arch = "wasm32"))]
		{
			let files = self.files.shared_handle();
			let effective = self.effective.cloned();
			let name = name.to_owned();
			match tokio::spawn(async move {
				let store = RefStore::<_, H>::new(&files).with_effective_config(effective.as_ref());
				store.publish_optional_ref_inline(&name, &state).await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}

		#[cfg(target_arch = "wasm32")]
		self.publish_optional_ref_inline(name, &state).await
	}

	async fn publish_optional_ref_inline(
		&self,
		name: &str,
		state: &HeadState<H>,
	) -> Result<bool, RepositoryError> {
		// A loose ancestor ref prevents the target lock's parent directory from being created. Detect
		// that namespace shape before locking so an optional ref remains optional rather than turning a
		// completed fetch into an error.
		if self.path_write_blocked(name).await? {
			return Ok(false);
		}
		let acquired = match self
			.lock_all(&[name.to_owned(), PACKED_REFS.to_owned()])
			.await
		{
			Ok(acquired) => acquired,
			Err((failed, error)) => {
				// An ancestor may have become a loose ref after the preflight. Confirm that concrete
				// namespace conflict before translating the target-lock error; unrelated storage and
				// contention failures remain fatal.
				if failed == name && self.path_write_blocked(name).await? {
					return Ok(false);
				}
				return Err(error);
			}
		};
		let result = self.publish_optional_ref_locked(name, state).await;
		let HeldRefLocks { names, locks } = acquired;
		drop(locks);
		for name in &names {
			self.prune_empty_dirs(name).await;
		}
		result
	}

	async fn publish_optional_ref_locked(
		&self,
		name: &str,
		state: &HeadState<H>,
	) -> Result<bool, RepositoryError> {
		let packed_refs = self.read_opt(PACKED_REFS).await?;
		if self
			.ref_path_write_blocked(name, packed_refs.as_deref())
			.await?
		{
			return Ok(false);
		}
		if let Some(packed_refs) = packed_refs.as_deref()
			&& self
				.packed_ref_name_resolves_through_alias(name, packed_refs)
				.await?
		{
			return Ok(false);
		}
		if self.ref_name_resolves_through_alias(name).await? {
			return Ok(false);
		}
		match self.files.read_path(name).await {
			Ok(bytes) => match HeadState::<H>::parse(&bytes) {
				Ok(HeadState::Symbolic(target)) if !is_valid_refname(&target) => return Ok(false),
				Ok(_) => {}
				Err(_) => return Ok(false),
			},
			Err(FileStoreError::NotFound) => {
				// Validate a packed exact occupant before shadowing it with the optional loose ref.
				self.resolve_packed(name).await?;
			}
			Err(other) => return Err(other.into()),
		}
		self
			.files
			.write_path_replace(name, state.render().as_bytes())
			.await?;
		Ok(true)
	}

	/// Whether `name` resolves to a loose entry whose directory spelling differs from the requested
	/// name. This catches case and normalization aliases on filesystems that identify those spellings,
	/// while permitting both refs on a filesystem where they are genuinely distinct. The caller holds
	/// the ref lock, whose differently spelled aliases resolve to the same lock on such filesystems.
	async fn ref_name_resolves_through_alias(&self, name: &str) -> Result<bool, RepositoryError> {
		let Some((parent, _)) = name.rsplit_once('/') else {
			return Ok(false);
		};
		let prefix = format!("{parent}/");
		let entries = self.files.list_prefix(&prefix).await?;
		if entries.iter().any(|entry| entry == name) {
			return Ok(false);
		}
		Ok(self.files.exists(name).await?)
	}

	/// Whether a packed ref occupies `name`, an ancestor, or a descendant through a different
	/// spelling that the native filesystem resolves to the same namespace. The caller holds
	/// `<name>.lock` and `packed-refs.lock`; spelling the held lock through each packed candidate lets
	/// the filesystem decide case and normalization equivalence without imposing global folding rules.
	async fn packed_ref_name_resolves_through_alias(
		&self,
		name: &str,
		packed_refs: &[u8],
	) -> Result<bool, RepositoryError> {
		let target_parts = name.split('/').collect::<Vec<_>>();
		for line in packed_refs.split(|byte| *byte == b'\n') {
			let line = line.strip_suffix(b"\r").unwrap_or(line);
			if line.starts_with(b"#") || line.starts_with(b"^") || line.is_empty() {
				continue;
			}
			let Some((_, packed_name)) = split_packed_ref(line) else {
				continue;
			};
			let Ok(packed_name) = std::str::from_utf8(packed_name) else {
				continue;
			};
			if packed_name == name || !is_valid_refname(packed_name) {
				continue;
			}
			let packed_parts = packed_name.split('/').collect::<Vec<_>>();
			let shared_depth = target_parts.len().min(packed_parts.len());
			let mut probe_parts = packed_parts[..shared_depth].to_vec();
			probe_parts.extend_from_slice(&target_parts[shared_depth..]);
			let probe = format!("{}.lock", probe_parts.join("/"));
			if self.files.exists(&probe).await? {
				return Ok(true);
			}
		}
		Ok(false)
	}

	async fn set_symbolic_inline(
		&self,
		name: &str,
		target: &str,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		// Hold `<name>.lock` across the reflog write and the retarget — like a ref transaction, so a
		// reflog failure leaves the symbolic ref unchanged and no concurrent writer interleaves. A
		// symbolic ref under `refs/` also takes `packed-refs.lock`, keeping its namespace validation
		// stable and excluding packed-ref rewrites until publication completes.
		let mut lock_names = vec![name.to_owned()];
		if name.starts_with("refs/") {
			lock_names.push(PACKED_REFS.to_owned());
		}
		let acquired = self
			.lock_all(&lock_names)
			.await
			.map_err(|(_, error)| error)?;
		let result = self.set_symbolic_locked(name, target, reflog).await;
		let HeldRefLocks { names, locks } = acquired;
		drop(locks);
		for name in &names {
			self.prune_empty_dirs(name).await;
		}
		result
	}

	/// The body of [`set_symbolic`](Self::set_symbolic), run with `<name>.lock` and, for a name under
	/// `refs/`, `packed-refs.lock` held.
	async fn set_symbolic_locked(
		&self,
		name: &str,
		target: &str,
		reflog: ReflogIntent<'_>,
	) -> Result<(), RepositoryError> {
		// Preflight the destination's writability before appending any reflog (as `transact` does): a
		// directory/file conflict at `name` (or, when logged, at `logs/<name>`) must reject the retarget
		// rather than record a reflog for a move that then fails on the ref write.
		let packed_refs = if name.starts_with("refs/") {
			self.read_opt("packed-refs").await?
		} else {
			None
		};
		if self
			.ref_path_write_blocked(name, packed_refs.as_deref())
			.await?
		{
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
		let path = format!("logs/{refname}");
		let mut content = match self.files.read_path(&path).await {
			Ok(bytes) => bytes,
			Err(FileStoreError::NotFound) => Vec::new(),
			Err(other) => return Err(other.into()),
		};
		content.extend_from_slice(&reflog_line(old, new, committer, message));
		self.force_write(&path, &content).await
	}

	/// Append the reflog line(s) for an [`update_ref`](Self::update_ref) that changed `name` from
	/// `old` to `new`, when `core.logAllRefUpdates` gating permits. When `name` is a branch that
	/// `HEAD` symbolically points at, git also mirrors the entry into `HEAD`'s reflog — the "split
	/// HEAD update" — so this cascades there too (each subject to its own gating).
	#[allow(clippy::too_many_arguments)]
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
	/// default (on for a non-bare repo, off for a bare one). A malformed explicit value is rejected;
	/// only an absent value falls back to the repository-kind default.
	///
	/// `logallrefupdates` follows git's merged precedence when the frontend installed the effective
	/// config (a global `true` enables reflogs); the `core.bare` fallback stays repo-local, matching
	/// the rest of gitana — a *global* `core.bare` is a footgun, so it is not honoured.
	async fn reflog_policy(&self) -> Result<ReflogPolicy, RepositoryError> {
		// The raw common config is the fallback for `core.bare`, and the `logallrefupdates` source when
		// no effective (merged) config was installed (tests, the wasm sandbox). A native effective
		// config carries the complete common/worktree-local repository range even when the common file
		// is a symlink outside this file-store capability.
		let local = match self.files.read_path("config").await {
			Ok(bytes) => std::str::from_utf8(&bytes)
				.ok()
				.and_then(|text| gitana_config::GitConfig::parse(text).ok()),
			Err(FileStoreError::NotFound) => None,
			// A native frontend may have followed a repository config symlink whose target is outside
			// this file-store capability and installed the resulting effective stack. In that case the
			// effective view is authoritative; do not turn a safe read boundary into a repository error.
			Err(_) if self.effective.is_some() => None,
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
		match config
			.get_bool("core", None, "logallrefupdates")
			.map_err(|error| {
				RepositoryError::UnsupportedFormat(format!("core.logAllRefUpdates: {error}"))
			})? {
			Some(true) => Ok(ReflogPolicy::Enabled),
			Some(false) => Ok(ReflogPolicy::Disabled),
			// Unset: git's default keys off whether the repo is bare. Resolve the winner across the
			// repository-owned common and worktree layers only; global and command values remain excluded.
			None => {
				let bare = self
					.effective
					.and_then(|config| {
						config
							.get_repository_bool("core", None, "bare")
							.ok()
							.flatten()
					})
					.or_else(|| {
						local
							.as_ref()
							.and_then(|config| config.get_bool("core", None, "bare").ok().flatten())
					})
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

fn reflog_line<H: HashAlgorithm>(
	old: Option<ObjectId<H>>,
	new: Option<ObjectId<H>>,
	committer: &str,
	message: &str,
) -> Vec<u8> {
	let zero = || "0".repeat(H::RAW_LEN * 2);
	let old = old.map_or_else(zero, |id| id.to_hex());
	let new = new.map_or_else(zero, |id| id.to_hex());
	if message.is_empty() {
		format!("{old} {new} {committer}\n").into_bytes()
	} else {
		format!("{old} {new} {committer}\t{message}\n").into_bytes()
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

/// Whether a name obeys Git's refname rules, including valid one-level symbolic-ref targets.
fn is_valid_refname(name: &str) -> bool {
	if name.is_empty() || name == "@" {
		return false;
	}
	if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
		return false;
	}
	if name.contains("..") || name.contains("@{") || name.ends_with('.') {
		return false;
	}
	if name.bytes().any(|byte| {
		byte < 0x20
			|| byte == 0x7f
			|| matches!(byte, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
	}) {
		return false;
	}
	name
		.split('/')
		.all(|component| !component.starts_with('.') && !component.ends_with(".lock"))
}

fn split_packed_ref(line: &[u8]) -> Option<(&[u8], &[u8])> {
	let separator = line.iter().position(|byte| *byte == b' ')?;
	Some((&line[..separator], &line[separator + 1..]))
}

fn packed_ref_path_conflict(packed_refs: &[u8], target: &str) -> bool {
	let target = target.as_bytes();
	packed_refs.split(|byte| *byte == b'\n').any(|line| {
		let line = line.strip_suffix(b"\r").unwrap_or(line);
		if line.starts_with(b"#") || line.starts_with(b"^") || line.is_empty() {
			return false;
		}
		let Some((_, name)) = split_packed_ref(line) else {
			return false;
		};
		strict_ref_prefix(name, target) || strict_ref_prefix(target, name)
	})
}

fn strict_ref_prefix(prefix: &[u8], value: &[u8]) -> bool {
	value
		.strip_prefix(prefix)
		.is_some_and(|suffix| suffix.starts_with(b"/"))
}

fn remove_prefix_lock_names(snapshot: &PrefixSnapshot) -> Vec<String> {
	let mut names: Vec<String> = snapshot
		.ref_files
		.iter()
		.map(|(path, _)| path.clone())
		.chain(
			snapshot
				.reflog_files
				.iter()
				.filter_map(|(path, _)| path.strip_prefix("logs/").map(str::to_owned)),
		)
		.chain(snapshot.packed_refs.iter().cloned())
		.collect();
	names.push(PACKED_REFS.to_owned());
	names
}

fn rename_prefix_lock_names(snapshot: &PrefixSnapshot, old: &str, new: &str) -> Vec<String> {
	let mut names = Vec::new();
	for (source, _) in &snapshot.ref_files {
		names.push(source.clone());
		names.push(format!("{new}{}", &source[old.len()..]));
	}
	let old_logs = format!("logs/{old}");
	let new_logs = format!("logs/{new}");
	for (source, _) in &snapshot.reflog_files {
		if let Some(source_ref) = source.strip_prefix("logs/") {
			names.push(source_ref.to_owned());
		}
		let target = format!("{new_logs}{}", &source[old_logs.len()..]);
		if let Some(target_ref) = target.strip_prefix("logs/") {
			names.push(target_ref.to_owned());
		}
	}
	for source in &snapshot.packed_refs {
		names.push(source.clone());
		names.push(format!("{new}{}", &source[old.len()..]));
	}
	names.push(PACKED_REFS.to_owned());
	names
}

fn locks_cover(acquired: &HeldRefLocks, required: &[String]) -> bool {
	let acquired: std::collections::HashSet<&str> =
		acquired.names.iter().map(String::as_str).collect();
	required.iter().all(|name| acquired.contains(name.as_str()))
}

#[cfg(test)]
mod tests {
	#[cfg(not(target_arch = "wasm32"))]
	use std::future::Future;
	#[cfg(not(target_arch = "wasm32"))]
	use std::task::{Context, Poll, Waker};

	use gitana_file_store::{FileStore, FileStoreError};
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::{ObjectId, ObjectKind, Sha256};

	#[cfg(not(target_arch = "wasm32"))]
	use crate::GatedFileStore;

	use super::{HeadState, PACKED_REFS, RefStore, ReflogIntent};

	#[cfg(not(target_arch = "wasm32"))]
	async fn let_retained_mutation_reach_a_held_lock() {
		tokio::task::spawn_blocking(|| {
			std::thread::sleep(std::time::Duration::from_millis(50));
		})
		.await
		.expect("wait task completes");
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn packed_snapshot_failures_are_attributed_to_a_requested_ref() {
		let files = GatedFileStore::new();
		files.fail_packed_reads();
		let store: RefStore<'_, GatedFileStore, Sha256> = RefStore::new(&files);
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		let ops = [
			crate::RefOp {
				name: "refs/heads/main".to_owned(),
				expected: None,
				new: Some(target),
				reflog: ReflogIntent::Skip,
			},
			crate::RefOp {
				name: "refs/heads/feature".to_owned(),
				expected: None,
				new: Some(target),
				reflog: ReflogIntent::Skip,
			},
		];

		let (name, error) = store
			.transact(&ops)
			.await
			.expect_err("the packed snapshot read must fail");
		assert_eq!(name, "refs/heads/main");
		assert!(matches!(error, crate::RepositoryError::FileStore(_)));
	}

	#[tokio::test]
	async fn resolves_a_packed_ref_without_decoding_unrelated_names() {
		let files = MemoryFileStore::new();
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		let mut packed = format!("{} refs/heads/main\n", target.to_hex()).into_bytes();
		packed.extend_from_slice(format!("{} refs/heads/", "f".repeat(64)).as_bytes());
		packed.extend_from_slice(b"other-\xff\n");
		files
			.write_path_if_absent("packed-refs", &packed)
			.await
			.unwrap();

		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		assert_eq!(
			store.resolve("refs/heads/main").await.unwrap(),
			Some(target)
		);
	}

	#[tokio::test]
	async fn packed_ref_listing_decodes_only_the_requested_namespace() {
		let files = MemoryFileStore::new();
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		let mut packed = format!("{} refs/heads/main\n", target.to_hex()).into_bytes();
		packed.extend_from_slice(format!("{} refs/tags/", "f".repeat(64)).as_bytes());
		packed.extend_from_slice(b"other-\xff\n");
		files
			.write_path_if_absent("packed-refs", &packed)
			.await
			.unwrap();

		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		assert_eq!(
			store.list("refs/heads/").await.unwrap(),
			vec![("refs/heads/main".to_owned(), target)]
		);
		assert!(matches!(
			store.list("refs/tags/").await,
			Err(crate::RepositoryError::InvalidRef(_))
		));
	}

	#[tokio::test]
	async fn flat_store_ref_transactions_reject_loose_descendant_conflicts_atomically() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let child = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"child");
		let proposed = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"proposed");
		store
			.update_ref("refs/tags/topic/child", child, None, ReflogIntent::Skip)
			.await
			.unwrap();

		let ops = [
			crate::RefOp {
				name: "refs/tags/clean".to_owned(),
				expected: None,
				new: Some(proposed),
				reflog: ReflogIntent::Skip,
			},
			crate::RefOp {
				name: "refs/tags/topic".to_owned(),
				expected: None,
				new: Some(proposed),
				reflog: ReflogIntent::Skip,
			},
		];
		let (name, error) = store
			.transact(&ops)
			.await
			.expect_err("a loose descendant must block its parent on a flat store");
		assert_eq!(name, "refs/tags/topic");
		assert!(matches!(error, crate::RepositoryError::InvalidRef(_)));
		assert_eq!(store.resolve("refs/tags/clean").await.unwrap(), None);
		assert_eq!(store.resolve("refs/tags/topic").await.unwrap(), None);
		assert_eq!(
			store.resolve("refs/tags/topic/child").await.unwrap(),
			Some(child)
		);
	}

	#[tokio::test]
	async fn optional_ref_publication_writes_direct_and_symbolic_states() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let first = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"first");
		let second = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"second");
		files
			.write_path_if_absent("refs/remotes/origin/main", format!("{first}\n").as_bytes())
			.await
			.unwrap();

		assert!(
			store
				.publish_optional_ref(
					"refs/remotes/origin/HEAD",
					HeadState::Symbolic("refs/remotes/origin/main".to_owned()),
				)
				.await
				.unwrap()
		);
		assert_eq!(
			store
				.resolve_symbolic("refs/remotes/origin/HEAD")
				.await
				.unwrap(),
			Some(first)
		);
		assert!(
			store
				.publish_optional_ref("refs/remotes/origin/HEAD", HeadState::Detached(second),)
				.await
				.unwrap()
		);
		assert_eq!(
			store
				.resolve_symbolic("refs/remotes/origin/HEAD")
				.await
				.unwrap(),
			Some(second)
		);
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn optional_ref_publication_preserves_a_filesystem_equivalent_spelling() {
		use gitana_file_store_local::LocalFileStore;

		let tmp =
			std::env::temp_dir().join(format!("gitana-optional-ref-alias-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(&tmp).unwrap();
		let files = LocalFileStore::from_dir(
			cap_std::fs::Dir::open_ambient_dir(&tmp, cap_std::ambient_authority()).unwrap(),
		);
		let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);
		let branch = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"branch");
		let convenience = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"convenience");
		store
			.update_ref("refs/remotes/origin/head", branch, None, ReflogIntent::Skip)
			.await
			.unwrap();

		if !tmp.join("refs/remotes/origin/HEAD").exists() {
			let _ = std::fs::remove_dir_all(&tmp);
			return;
		}
		assert_eq!(
			store
				.resolve_symbolic_exact("refs/remotes/origin/HEAD")
				.await
				.unwrap(),
			None,
			"an aliased tracking branch must not satisfy the exact convenience-ref name"
		);
		assert_eq!(
			store
				.resolve_symbolic_exact("refs/remotes/origin/head")
				.await
				.unwrap(),
			Some(branch)
		);
		assert!(
			!store
				.publish_optional_ref("refs/remotes/origin/HEAD", HeadState::Detached(convenience),)
				.await
				.unwrap()
		);
		assert_eq!(
			store
				.resolve_symbolic("refs/remotes/origin/head")
				.await
				.unwrap(),
			Some(branch)
		);
		let _ = std::fs::remove_dir_all(&tmp);
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn optional_ref_publication_preserves_a_loose_ancestor() {
		use gitana_file_store_local::LocalFileStore;

		let tmp = std::env::temp_dir().join(format!(
			"gitana-optional-ref-loose-ancestor-{}",
			std::process::id()
		));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(tmp.join("refs/remotes")).unwrap();
		let ancestor = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"ancestor");
		std::fs::write(tmp.join("refs/remotes/origin"), format!("{ancestor}\n")).unwrap();
		let files = LocalFileStore::from_dir(
			cap_std::fs::Dir::open_ambient_dir(&tmp, cap_std::ambient_authority()).unwrap(),
		);
		let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);
		let proposed = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"proposed");

		assert!(
			!store
				.publish_optional_ref("refs/remotes/origin/HEAD", HeadState::Detached(proposed),)
				.await
				.unwrap()
		);
		assert_eq!(
			store.resolve("refs/remotes/origin").await.unwrap(),
			Some(ancestor)
		);
		assert!(!tmp.join("refs/remotes/origin/HEAD.lock").exists());
		let _ = std::fs::remove_dir_all(&tmp);
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn optional_ref_publication_preserves_filesystem_equivalent_packed_namespaces() {
		use gitana_file_store_local::LocalFileStore;

		let tmp = std::env::temp_dir().join(format!(
			"gitana-optional-packed-ref-alias-{}",
			std::process::id()
		));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(&tmp).unwrap();
		std::fs::write(tmp.join("case-probe"), b"").unwrap();
		if !tmp.join("CASE-PROBE").exists() {
			let _ = std::fs::remove_dir_all(&tmp);
			return;
		}

		for (index, (packed_name, requested)) in [
			("refs/remotes/origin/head", "refs/remotes/origin/HEAD"),
			("refs/remotes/origin/head/child", "refs/remotes/origin/HEAD"),
			("refs/remotes/origin/head", "refs/remotes/origin/HEAD/child"),
		]
		.into_iter()
		.enumerate()
		{
			let case = tmp.join(index.to_string());
			std::fs::create_dir_all(&case).unwrap();
			let files = LocalFileStore::from_dir(
				cap_std::fs::Dir::open_ambient_dir(&case, cap_std::ambient_authority()).unwrap(),
			);
			let packed = ObjectId::<Sha256>::compute(ObjectKind::Commit, packed_name.as_bytes());
			let proposed = ObjectId::<Sha256>::compute(ObjectKind::Commit, requested.as_bytes());
			files
				.write_path_if_absent(PACKED_REFS, format!("{packed} {packed_name}\n").as_bytes())
				.await
				.unwrap();
			let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);

			assert!(
				!store
					.publish_optional_ref(requested, HeadState::Detached(proposed))
					.await
					.unwrap(),
				"{packed_name} must preserve its filesystem-equivalent namespace from {requested}"
			);
			assert_eq!(store.resolve(packed_name).await.unwrap(), Some(packed));
		}
		let _ = std::fs::remove_dir_all(&tmp);
	}

	#[tokio::test]
	async fn optional_ref_publication_keeps_case_distinct_packed_names_distinct() {
		let files = MemoryFileStore::new();
		let packed = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"packed");
		let proposed = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"proposed");
		files
			.write_path_if_absent(
				PACKED_REFS,
				format!("{packed} refs/remotes/origin/head\n").as_bytes(),
			)
			.await
			.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);

		assert!(
			store
				.publish_optional_ref("refs/remotes/origin/HEAD", HeadState::Detached(proposed),)
				.await
				.unwrap()
		);
		assert_eq!(
			store.resolve("refs/remotes/origin/head").await.unwrap(),
			Some(packed)
		);
		assert_eq!(
			store.resolve("refs/remotes/origin/HEAD").await.unwrap(),
			Some(proposed)
		);
	}

	#[tokio::test]
	async fn optional_ref_publication_preserves_blocked_and_malformed_occupants() {
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");

		for bytes in [b"not a ref\n".as_slice(), b"ref: bad ref\n".as_slice()] {
			let malformed = MemoryFileStore::new();
			malformed
				.write_path_if_absent("refs/remotes/origin/HEAD", bytes)
				.await
				.unwrap();
			let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&malformed);
			assert!(
				!store
					.publish_optional_ref("refs/remotes/origin/HEAD", HeadState::Detached(target),)
					.await
					.unwrap()
			);
			assert_eq!(
				malformed
					.read_path("refs/remotes/origin/HEAD")
					.await
					.unwrap(),
				bytes
			);
		}

		let blocked = MemoryFileStore::new();
		blocked
			.write_path_if_absent(
				"refs/remotes/origin/HEAD/topic",
				format!("{target}\n").as_bytes(),
			)
			.await
			.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&blocked);
		assert!(
			!store
				.publish_optional_ref("refs/remotes/origin/HEAD", HeadState::Detached(target),)
				.await
				.unwrap()
		);
		assert_eq!(
			blocked
				.read_path("refs/remotes/origin/HEAD/topic")
				.await
				.unwrap(),
			format!("{target}\n").as_bytes()
		);
	}

	#[tokio::test]
	async fn packed_ref_ancestors_and_descendants_block_loose_writes() {
		let existing = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"existing");
		let proposed = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"proposed");
		let cases = [
			(b"refs/tags/topic/child".as_slice(), "refs/tags/topic"),
			(b"refs/tags/topic".as_slice(), "refs/tags/topic/child"),
			(b"refs/tags/topic/\xff".as_slice(), "refs/tags/topic"),
		];

		for (packed_name, requested) in cases {
			let files = MemoryFileStore::new();
			let mut packed = format!("{} ", existing.to_hex()).into_bytes();
			packed.extend_from_slice(packed_name);
			packed.push(b'\n');
			files
				.write_path_if_absent("packed-refs", &packed)
				.await
				.unwrap();
			let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);

			let error = store
				.update_ref(requested, proposed, None, ReflogIntent::Skip)
				.await
				.expect_err("a packed directory/file conflict must reject the write");
			assert!(matches!(error, crate::RepositoryError::InvalidRef(_)));
			assert!(matches!(
				files.read_path(requested).await,
				Err(FileStoreError::NotFound)
			));
		}
	}

	#[tokio::test]
	async fn an_exact_packed_ref_can_still_be_shadowed_by_a_loose_update() {
		let files = MemoryFileStore::new();
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let new = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"new");
		files
			.write_path_if_absent(
				"packed-refs",
				format!("{} refs/tags/topic\n", old.to_hex()).as_bytes(),
			)
			.await
			.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);

		store
			.update_ref("refs/tags/topic", new, Some(old), ReflogIntent::Skip)
			.await
			.expect("an exact packed ref is a valid update target");
		assert_eq!(store.resolve("refs/tags/topic").await.unwrap(), Some(new));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn ref_transactions_validate_after_a_concurrent_packed_ref_rewrite() {
		let files = MemoryFileStore::new();
		let existing = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"existing");
		let proposed = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"proposed");
		let packed_lock = files
			.try_lock_path("packed-refs.lock")
			.await
			.unwrap()
			.expect("simulate pack-refs owning its lock");

		let worker_files = files.shared_handle();
		let update = tokio::spawn(async move {
			let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&worker_files);
			store
				.update_ref("refs/tags/topic", proposed, None, ReflogIntent::Skip)
				.await
		});
		let_retained_mutation_reach_a_held_lock().await;
		assert!(
			!update.is_finished(),
			"the ref transaction must wait for packed-refs.lock"
		);

		// The packer publishes a child while it owns the lock. Once the transaction acquires the lock,
		// it must read this new table and reject the now-conflicting parent rather than use a stale view.
		files
			.write_path_replace(
				PACKED_REFS,
				format!("{} refs/tags/topic/child\n", existing.to_hex()).as_bytes(),
			)
			.await
			.unwrap();
		drop(packed_lock);

		let error = update
			.await
			.expect("update task completes")
			.expect_err("the newly packed child blocks its parent");
		assert!(matches!(error, crate::RepositoryError::InvalidRef(_)));
		assert!(matches!(
			files.read_path("refs/tags/topic").await,
			Err(FileStoreError::NotFound)
		));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn symbolic_refs_validate_after_a_concurrent_packed_ref_rewrite() {
		let files = MemoryFileStore::new();
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		files
			.write_path_if_absent("refs/heads/main", format!("{target}\n").as_bytes())
			.await
			.unwrap();
		let packed_lock = files
			.try_lock_path("packed-refs.lock")
			.await
			.unwrap()
			.expect("simulate pack-refs owning its lock");

		let worker_files = files.shared_handle();
		let update = tokio::spawn(async move {
			let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&worker_files);
			store
				.set_symbolic("refs/tags/topic", "refs/heads/main", ReflogIntent::Skip)
				.await
		});
		let_retained_mutation_reach_a_held_lock().await;
		assert!(
			!update.is_finished(),
			"the symbolic update must wait for packed-refs.lock"
		);

		files
			.write_path_replace(
				PACKED_REFS,
				format!("{target} refs/tags/topic/child\n").as_bytes(),
			)
			.await
			.unwrap();
		drop(packed_lock);

		let error = update
			.await
			.expect("symbolic update task completes")
			.expect_err("the newly packed child blocks its parent");
		assert!(matches!(error, crate::RepositoryError::InvalidRef(_)));
		assert!(matches!(
			files.read_path("refs/tags/topic").await,
			Err(FileStoreError::NotFound)
		));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn packed_ref_prefix_rewriters_wait_for_the_shared_lock() {
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");

		let remove_files = MemoryFileStore::new();
		let remove_lock = remove_files
			.try_lock_path("packed-refs.lock")
			.await
			.unwrap()
			.expect("simulate a concurrent packed-ref writer");
		let worker_files = remove_files.shared_handle();
		let remove = tokio::spawn(async move {
			let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&worker_files);
			store.remove_prefix("refs/remotes/origin/").await
		});
		let_retained_mutation_reach_a_held_lock().await;
		assert!(
			!remove.is_finished(),
			"prefix removal must wait for packed-refs.lock"
		);
		remove_files
			.write_path_replace(
				PACKED_REFS,
				format!("{tip} refs/remotes/origin/main\n").as_bytes(),
			)
			.await
			.unwrap();
		drop(remove_lock);
		remove
			.await
			.expect("remove task completes")
			.expect("remove the newly packed source");
		assert!(
			!String::from_utf8(remove_files.read_path(PACKED_REFS).await.unwrap())
				.unwrap()
				.contains("refs/remotes/origin/main")
		);

		let rename_files = MemoryFileStore::new();
		let rename_lock = rename_files
			.try_lock_path("packed-refs.lock")
			.await
			.unwrap()
			.expect("simulate a concurrent packed-ref writer");
		let worker_files = rename_files.shared_handle();
		let rename = tokio::spawn(async move {
			let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&worker_files);
			store
				.rename_prefix("refs/remotes/origin/", "refs/remotes/upstream/")
				.await
		});
		let_retained_mutation_reach_a_held_lock().await;
		assert!(
			!rename.is_finished(),
			"prefix rename must wait for packed-refs.lock"
		);
		rename_files
			.write_path_replace(
				PACKED_REFS,
				format!("{tip} refs/remotes/origin/main\n").as_bytes(),
			)
			.await
			.unwrap();
		drop(rename_lock);
		rename
			.await
			.expect("rename task completes")
			.expect("rename the newly packed source");
		let packed = String::from_utf8(rename_files.read_path(PACKED_REFS).await.unwrap()).unwrap();
		assert!(packed.contains("refs/remotes/upstream/main"));
		assert!(!packed.contains("refs/remotes/origin/main"));
	}

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
	async fn update_ref_following_symbolic_logs_every_hop_and_preserves_the_chain() {
		let files = MemoryFileStore::new();
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let new = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"new");
		let committer = "A U Thor <a@example.com> 1700000000 +0000";
		files
			.write_path_if_absent("refs/remotes/origin/main", format!("{old}\n").as_bytes())
			.await
			.unwrap();
		files
			.write_path_if_absent(
				"refs/remotes/origin/alias",
				b"ref: refs/remotes/origin/main\n",
			)
			.await
			.unwrap();
		files
			.write_path_if_absent(
				"refs/remotes/origin/HEAD",
				b"ref: refs/remotes/origin/alias\n",
			)
			.await
			.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);

		store
			.update_ref_following_symbolic(
				"refs/remotes/origin/HEAD",
				new,
				Some(old),
				ReflogIntent::Log {
					committer,
					message: "fetch: fast-forward",
				},
			)
			.await
			.expect("update the symbolic destination's referent");

		assert_eq!(
			store
				.read_symbolic("refs/remotes/origin/HEAD")
				.await
				.unwrap()
				.as_deref(),
			Some("refs/remotes/origin/alias")
		);
		assert_eq!(
			store
				.read_symbolic("refs/remotes/origin/alias")
				.await
				.unwrap()
				.as_deref(),
			Some("refs/remotes/origin/main")
		);
		assert_eq!(
			store.resolve("refs/remotes/origin/main").await.unwrap(),
			Some(new)
		);
		assert_eq!(
			store
				.resolve_symbolic("refs/remotes/origin/HEAD")
				.await
				.unwrap(),
			Some(new)
		);
		let moved = format!("{old} {new} {committer}\tfetch: fast-forward\n");
		for name in ["HEAD", "alias", "main"] {
			assert_eq!(
				files
					.read_path(&format!("logs/refs/remotes/origin/{name}"))
					.await
					.unwrap(),
				moved.as_bytes(),
				"the real move must log {name}"
			);
		}

		store
			.update_ref_following_symbolic(
				"refs/remotes/origin/HEAD",
				new,
				Some(new),
				ReflogIntent::Log {
					committer,
					message: "fetch: no-op",
				},
			)
			.await
			.expect("log a no-op through the symbolic chain");
		let no_op = format!("{new} {new} {committer}\tfetch: no-op\n");
		for name in ["HEAD", "alias"] {
			assert_eq!(
				files
					.read_path(&format!("logs/refs/remotes/origin/{name}"))
					.await
					.unwrap(),
				format!("{moved}{no_op}").as_bytes(),
				"the symbolic no-op must log {name}"
			);
		}
		assert_eq!(
			files
				.read_path("logs/refs/remotes/origin/main")
				.await
				.unwrap(),
			moved.as_bytes(),
			"the terminal direct ref must not log a no-op"
		);
	}

	#[tokio::test]
	async fn update_ref_following_head_logs_the_head_cascade_once() {
		let files = MemoryFileStore::new();
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let new = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"new");
		let committer = "A U Thor <a@example.com> 1700000000 +0000";
		files
			.write_path_if_absent("HEAD", b"ref: refs/heads/main\n")
			.await
			.unwrap();
		files
			.write_path_if_absent("refs/heads/main", format!("{old}\n").as_bytes())
			.await
			.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);

		store
			.update_ref_following_symbolic(
				"HEAD",
				new,
				Some(old),
				ReflogIntent::Log {
					committer,
					message: "reset: moving to target",
				},
			)
			.await
			.unwrap();

		let line = format!("{old} {new} {committer}\treset: moving to target\n");
		for name in ["HEAD", "refs/heads/main"] {
			assert_eq!(
				files.read_path(&format!("logs/{name}")).await.unwrap(),
				line.as_bytes(),
				"{name} must receive exactly one entry"
			);
		}
	}

	#[tokio::test]
	async fn update_ref_following_symbolic_preflights_every_hop_reflog() {
		let files = MemoryFileStore::new();
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let new = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"new");
		files
			.write_path_if_absent("refs/remotes/origin/main", format!("{old}\n").as_bytes())
			.await
			.unwrap();
		files
			.write_path_if_absent(
				"refs/remotes/origin/alias",
				b"ref: refs/remotes/origin/main\n",
			)
			.await
			.unwrap();
		files
			.write_path_if_absent(
				"refs/remotes/origin/HEAD",
				b"ref: refs/remotes/origin/alias\n",
			)
			.await
			.unwrap();
		files
			.write_path_if_absent("logs/refs/remotes/origin/alias/blocker", b"occupied")
			.await
			.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);

		let error = store
			.update_ref_following_symbolic(
				"refs/remotes/origin/HEAD",
				new,
				Some(old),
				ReflogIntent::Log {
					committer: "A U Thor <a@example.com> 1700000000 +0000",
					message: "fetch: fast-forward",
				},
			)
			.await
			.expect_err("a blocked hop reflog must reject before publication");
		assert!(
			error
				.to_string()
				.contains("reflog path refs/remotes/origin/alias blocked")
		);
		assert_eq!(
			store.resolve("refs/remotes/origin/main").await.unwrap(),
			Some(old)
		);
		assert_eq!(
			files.read_path("refs/remotes/origin/HEAD").await.unwrap(),
			b"ref: refs/remotes/origin/alias\n"
		);
		assert!(!files.exists("logs/refs/remotes/origin/HEAD").await.unwrap());
		assert!(!files.exists("logs/refs/remotes/origin/main").await.unwrap());
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

	#[tokio::test]
	async fn packed_ref_lock_contention_is_attributed_to_an_operation() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let first = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"first");
		let second = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"second");
		let _packed_lock = files
			.try_lock_path("packed-refs.lock")
			.await
			.unwrap()
			.expect("hold the shared packed-ref lock");
		let ops = [
			crate::RefOp {
				name: "refs/heads/one".to_owned(),
				expected: None,
				new: Some(first),
				reflog: ReflogIntent::Skip,
			},
			crate::RefOp {
				name: "refs/heads/two".to_owned(),
				expected: None,
				new: Some(second),
				reflog: ReflogIntent::Skip,
			},
		];

		let (name, error) = store
			.transact(&ops)
			.await
			.expect_err("the shared lock remains contended");
		assert_eq!(
			name, "refs/heads/one",
			"transaction callers receive an actual operation name"
		);
		assert!(
			matches!(&error, crate::RepositoryError::RefLocked { name } if name == PACKED_REFS),
			"the underlying diagnostic still identifies packed-refs.lock: {error:?}"
		);
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn ref_transactions_acquire_ref_locks_before_packed_refs() {
		let files = MemoryFileStore::new();
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");
		let ref_lock = files
			.try_lock_path("refs/tags/topic.lock")
			.await
			.unwrap()
			.expect("simulate stock Git owning the ref lock");
		let worker_files = files.shared_handle();
		let update = tokio::spawn(async move {
			let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&worker_files);
			store
				.update_ref("refs/tags/topic", tip, None, ReflogIntent::Skip)
				.await
		});
		let_retained_mutation_reach_a_held_lock().await;
		assert!(!update.is_finished());
		assert!(
			!files.exists("packed-refs.lock").await.unwrap(),
			"a transaction waiting for a ref lock must not hold packed-refs.lock"
		);

		drop(ref_lock);
		update
			.await
			.expect("update task completes")
			.expect("update proceeds after the ref lock is released");
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn prefix_rewriters_acquire_ref_locks_before_packed_refs() {
		let files = MemoryFileStore::new();
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");
		files
			.write_path_if_absent("refs/remotes/origin/main", format!("{tip}\n").as_bytes())
			.await
			.unwrap();
		let ref_lock = files
			.try_lock_path("refs/remotes/origin/main.lock")
			.await
			.unwrap()
			.expect("simulate stock Git owning the ref lock");
		let worker_files = files.shared_handle();
		let remove = tokio::spawn(async move {
			let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&worker_files);
			store.remove_prefix("refs/remotes/origin/").await
		});
		let_retained_mutation_reach_a_held_lock().await;
		assert!(!remove.is_finished());
		assert!(
			!files.exists("packed-refs.lock").await.unwrap(),
			"a prefix rewrite waiting for a ref lock must not hold packed-refs.lock"
		);

		drop(ref_lock);
		remove
			.await
			.expect("remove task completes")
			.expect("remove proceeds after the ref lock is released");
		assert!(matches!(
			files.read_path("refs/remotes/origin/main").await,
			Err(FileStoreError::NotFound)
		));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn cancelled_multi_ref_publication_retains_locks_and_completes() {
		let files = GatedFileStore::new();
		let store: RefStore<'_, GatedFileStore, Sha256> = RefStore::new(&files);
		let first = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"first");
		let second = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"second");
		let successor = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"successor");
		let ops = [
			crate::RefOp {
				name: "refs/heads/a".to_owned(),
				expected: None,
				new: Some(first),
				reflog: ReflogIntent::Skip,
			},
			crate::RefOp {
				name: "refs/heads/b".to_owned(),
				expected: None,
				new: Some(second),
				reflog: ReflogIntent::Skip,
			},
		];
		let mut transaction = Box::pin(store.transact(&ops));
		let mut context = Context::from_waker(Waker::noop());
		assert!(matches!(
			transaction.as_mut().poll(&mut context),
			Poll::Pending
		));
		files.wait_until_blocked().await;
		assert!(
			files.exists("refs/heads/a.lock").await.unwrap()
				&& files.exists("refs/heads/b.lock").await.unwrap(),
			"the worker reached publication with every ref lock held",
		);

		drop(transaction);
		assert!(
			files.exists("refs/heads/a.lock").await.unwrap(),
			"dropping the waiter must not release the worker's ref lock",
		);

		let mut racing =
			Box::pin(store.update_ref("refs/heads/a", successor, None, ReflogIntent::Skip));
		assert!(matches!(racing.as_mut().poll(&mut context), Poll::Pending));
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert_eq!(store.resolve("refs/heads/a").await.unwrap(), None);
		assert!(files.exists("refs/heads/a.lock").await.unwrap());

		files.release();
		let error = racing
			.await
			.expect_err("the successor must observe the retained transaction's create");
		assert!(matches!(error, crate::RepositoryError::RefMoved { .. }));
		assert_eq!(store.resolve("refs/heads/a").await.unwrap(), Some(first));
		assert_eq!(store.resolve("refs/heads/b").await.unwrap(), Some(second));
		assert!(!files.exists("refs/heads/a.lock").await.unwrap());
		assert!(!files.exists("refs/heads/b.lock").await.unwrap());
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn cancelled_symbolic_publication_retains_its_lock() {
		let files = GatedFileStore::new();
		let store: RefStore<'_, GatedFileStore, Sha256> = RefStore::new(&files);
		let one = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"one");
		let two = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"two");
		files
			.write_path_if_absent("refs/heads/one", format!("{one}\n").as_bytes())
			.await
			.unwrap();
		files
			.write_path_if_absent("refs/heads/two", format!("{two}\n").as_bytes())
			.await
			.unwrap();

		let mut first = Box::pin(store.set_symbolic("HEAD", "refs/heads/one", ReflogIntent::Skip));
		let mut context = Context::from_waker(Waker::noop());
		assert!(matches!(first.as_mut().poll(&mut context), Poll::Pending));
		files.wait_until_blocked().await;
		drop(first);
		assert!(files.exists("HEAD.lock").await.unwrap());

		let mut second = Box::pin(store.set_symbolic("HEAD", "refs/heads/two", ReflogIntent::Skip));
		assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(files.exists("HEAD.lock").await.unwrap());
		assert!(!files.exists("HEAD").await.unwrap());

		files.release();
		second
			.await
			.expect("successor retargets after the retained worker");
		assert_eq!(
			files.read_path("HEAD").await.unwrap(),
			b"ref: refs/heads/two\n"
		);
		assert!(!files.exists("HEAD.lock").await.unwrap());
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

	/// Failure while acquiring a later lock releases and prunes directories created for earlier
	/// nested locks, rather than leaving the parent spelling permanently blocked.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn partial_lock_acquisition_prunes_nested_directories() {
		use gitana_file_store_local::LocalFileStore;

		let tmp = std::env::temp_dir().join(format!("gitana-partial-lockprune-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(&tmp).unwrap();
		let files = LocalFileStore::from_dir(
			cap_std::fs::Dir::open_ambient_dir(&tmp, cap_std::ambient_authority()).unwrap(),
		);
		let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");
		let blocked = files
			.try_lock_path("refs/heads/z.lock")
			.await
			.unwrap()
			.expect("reserve the later lock");
		let ops = [
			crate::RefOp {
				name: "refs/heads/a/nested".to_owned(),
				expected: None,
				new: Some(tip),
				reflog: ReflogIntent::Skip,
			},
			crate::RefOp {
				name: "refs/heads/z".to_owned(),
				expected: None,
				new: Some(tip),
				reflog: ReflogIntent::Skip,
			},
		];
		let (name, error) = store
			.transact(&ops)
			.await
			.expect_err("the later lock is contended");
		assert_eq!(name, "refs/heads/z");
		assert!(matches!(error, crate::RepositoryError::RefLocked { .. }));
		drop(blocked);

		store
			.update_ref("refs/heads/a", tip, None, ReflogIntent::Skip)
			.await
			.expect("the earlier nested lock directory was pruned");
		assert_eq!(store.resolve("refs/heads/a").await.unwrap(), Some(tip));

		let _ = std::fs::remove_dir_all(&tmp);
	}

	/// The `switch -c` self-cascade path: `finish_checkout` creates the branch its (unborn) `HEAD` points
	/// at and publishes `HEAD` in one step. That create cascades into `logs/HEAD`, so a bare transaction
	/// would re-lock the very `HEAD.lock` the checkout holds and deadlock; the checkout writes it under
	/// the held lock instead, then publishes `HEAD` and releases.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn finish_checkout_creates_the_branch_head_is_on_without_relocking() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");
		// HEAD on an unborn branch: creating it cascades into `logs/HEAD`, so it would join the lock set.
		store
			.set_symbolic("HEAD", "refs/heads/orphan", ReflogIntent::Skip)
			.await
			.unwrap();

		let head_lock = store.lock_head().await.unwrap();
		assert!(files.exists("HEAD.lock").await.unwrap());

		head_lock
			.finish_checkout(
				"refs/heads/orphan",
				Some((tip, ReflogIntent::Skip)),
				ReflogIntent::Skip,
			)
			.await
			.expect("create the branch HEAD is on and publish HEAD under the held lock, not deadlocking");
		assert_eq!(store.resolve("refs/heads/orphan").await.unwrap(), Some(tip));
		assert_eq!(
			files.read_path("HEAD").await.unwrap(),
			b"ref: refs/heads/orphan\n"
		);
		assert!(
			!files.exists("HEAD.lock").await.unwrap(),
			"publishing the checkout releases the HEAD.lock",
		);
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn finish_detached_publishes_the_target_and_releases_head_lock() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		store
			.update_ref("refs/heads/main", old, None, ReflogIntent::Skip)
			.await
			.unwrap();
		store
			.set_symbolic("HEAD", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();

		let head_lock = store.lock_head().await.unwrap();
		assert!(files.exists("HEAD.lock").await.unwrap());

		head_lock
			.finish_detached(target, ReflogIntent::Skip)
			.await
			.expect("publish detached HEAD under the held lock");
		assert_eq!(store.resolve_head().await.unwrap(), Some(target));
		assert_eq!(
			files.read_path("HEAD").await.unwrap(),
			format!("{}\n", target.to_hex()).as_bytes()
		);
		assert!(!files.exists("HEAD.lock").await.unwrap());
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn prepared_detached_head_validates_before_publication_and_releases_on_drop() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		store
			.update_ref("refs/heads/main", old, None, ReflogIntent::Skip)
			.await
			.unwrap();
		store
			.set_symbolic("HEAD", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();

		let prepared = store
			.lock_head()
			.await
			.unwrap()
			.prepare_detached(target, ReflogIntent::Skip)
			.await
			.unwrap();
		assert!(files.exists("HEAD.lock").await.unwrap());
		assert_eq!(store.resolve_head().await.unwrap(), Some(old));

		drop(prepared);
		assert!(!files.exists("HEAD.lock").await.unwrap());
		assert_eq!(store.resolve_head().await.unwrap(), Some(old));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn prepared_detached_head_rejects_reflog_conflicts_without_publishing() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		store
			.update_ref("refs/heads/main", old, None, ReflogIntent::Skip)
			.await
			.unwrap();
		store
			.set_symbolic("HEAD", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();
		files
			.write_path_replace("logs/HEAD/blocked", b"conflict")
			.await
			.unwrap();

		let error = store
			.lock_head()
			.await
			.unwrap()
			.prepare_detached(
				target,
				ReflogIntent::Log {
					committer: "A U Thor <a@u> 0 +0000",
					message: "checkout",
				},
			)
			.await
			.err()
			.expect("the reflog directory conflict must fail during preparation");
		assert!(matches!(error, crate::RepositoryError::InvalidRef(_)));
		assert!(!files.exists("HEAD.lock").await.unwrap());
		assert_eq!(store.resolve_head().await.unwrap(), Some(old));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn prepared_detached_head_rejects_a_malformed_effective_reflog_policy() {
		let files = MemoryFileStore::new();
		let raw: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		raw
			.update_ref("refs/heads/main", old, None, ReflogIntent::Skip)
			.await
			.unwrap();
		raw
			.set_symbolic("HEAD", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();
		let config =
			gitana_config::GitConfig::parse("[core]\n\tlogAllRefUpdates = definitely-not-a-boolean\n")
				.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> =
			RefStore::new(&files).with_effective_config(Some(&config));

		let error = store
			.lock_head()
			.await
			.unwrap()
			.prepare_detached(
				target,
				ReflogIntent::Log {
					committer: "A U Thor <a@u> 0 +0000",
					message: "checkout",
				},
			)
			.await
			.err()
			.expect("a malformed effective policy must fail during preparation");
		assert!(matches!(
			error,
			crate::RepositoryError::UnsupportedFormat(_)
		));
		assert!(!files.exists("HEAD.lock").await.unwrap());
		assert_eq!(store.resolve_head().await.unwrap(), Some(old));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn skipped_reflog_transaction_rejects_a_malformed_effective_policy_before_mutation() {
		let files = MemoryFileStore::new();
		let config =
			gitana_config::GitConfig::parse("[core]\n\tlogAllRefUpdates = definitely-not-a-boolean\n")
				.unwrap();
		let store: RefStore<'_, MemoryFileStore, Sha256> =
			RefStore::new(&files).with_effective_config(Some(&config));
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");

		let error = store
			.update_ref("refs/tags/v1", target, None, ReflogIntent::Skip)
			.await
			.expect_err("a malformed policy must be rejected even when the reflog is skipped");
		assert!(matches!(
			error,
			crate::RepositoryError::UnsupportedFormat(_)
		));
		assert_eq!(store.resolve("refs/tags/v1").await.unwrap(), None);
		assert!(!files.exists("refs/tags/v1.lock").await.unwrap());
	}

	/// Detached checkout publication has the same cancellation guarantee as branch checkout: the
	/// owned worker retains `HEAD.lock` until the direct HEAD write completes.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn cancelled_finish_detached_retains_head_lock_until_publication_completes() {
		let files = GatedFileStore::new();
		let store: RefStore<'_, GatedFileStore, Sha256> = RefStore::new(&files);
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"detached target");

		let head_lock = store.lock_head().await.unwrap();
		let mut publish = Box::pin(head_lock.finish_detached(target, ReflogIntent::Skip));
		let mut context = Context::from_waker(Waker::noop());
		assert!(matches!(publish.as_mut().poll(&mut context), Poll::Pending));
		files.wait_until_blocked().await;
		assert!(files.exists("HEAD.lock").await.unwrap());

		drop(publish);
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(files.exists("HEAD.lock").await.unwrap());
		assert!(!files.exists("HEAD").await.unwrap());

		files.release();
		for _ in 0..50 {
			tokio::task::yield_now().await;
			if !files.exists("HEAD.lock").await.unwrap() {
				break;
			}
		}
		assert_eq!(store.resolve_head().await.unwrap(), Some(target));
		assert!(!files.exists("HEAD.lock").await.unwrap());
	}

	/// Cancellation invariant: `finish_checkout` moves the `HEAD.lock` into its owned worker, so dropping
	/// the caller mid-publish must NOT release the lock — the worker runs to completion holding it, and
	/// only then is the branch published and the lock freed.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn cancelled_finish_checkout_retains_head_lock_until_the_worker_completes() {
		let files = GatedFileStore::new();
		let store: RefStore<'_, GatedFileStore, Sha256> = RefStore::new(&files);
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");

		let head_lock = store.lock_head().await.unwrap();
		let mut publish = Box::pin(head_lock.finish_checkout(
			"refs/heads/feature",
			Some((tip, ReflogIntent::Skip)),
			ReflogIntent::Skip,
		));
		let mut context = Context::from_waker(Waker::noop());
		assert!(matches!(publish.as_mut().poll(&mut context), Poll::Pending));
		files.wait_until_blocked().await;
		assert!(files.exists("HEAD.lock").await.unwrap());

		drop(publish);
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(
			files.exists("HEAD.lock").await.unwrap(),
			"the retained worker must keep HEAD.lock across caller cancellation",
		);
		assert_eq!(store.resolve("refs/heads/feature").await.unwrap(), None);

		files.release();
		for _ in 0..50 {
			tokio::task::yield_now().await;
			if !files.exists("HEAD.lock").await.unwrap() {
				break;
			}
		}
		assert_eq!(
			store.resolve("refs/heads/feature").await.unwrap(),
			Some(tip)
		);
		assert_eq!(
			files.read_path("HEAD").await.unwrap(),
			b"ref: refs/heads/feature\n"
		);
		assert!(!files.exists("HEAD.lock").await.unwrap());
	}

	/// A held [`HeadLock`] (as `switch` keeps across a checkout) excludes a ref transaction that moves the
	/// branch `HEAD` is on, because that move must lock `HEAD` for its reflog cascade. The branch cannot
	/// move out from under the checkout while the lock is held.
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn a_held_head_lock_excludes_a_cascading_branch_move() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");
		let next = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"next");
		store
			.update_ref("refs/heads/main", tip, None, ReflogIntent::Skip)
			.await
			.unwrap();
		store
			.set_symbolic("HEAD", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();

		let head_lock = store.lock_head().await.unwrap();
		assert!(files.exists("HEAD.lock").await.unwrap());

		let error = store
			.update_ref("refs/heads/main", next, Some(tip), ReflogIntent::Skip)
			.await
			.expect_err("a cascading move must contend on the checkout's held HEAD.lock");
		assert!(
			matches!(&error, crate::RepositoryError::RefLocked { name } if name == "HEAD"),
			"expected HEAD.lock contention, got {error:?}",
		);
		assert_eq!(store.resolve("refs/heads/main").await.unwrap(), Some(tip));

		drop(head_lock);
		store
			.update_ref("refs/heads/main", next, Some(tip), ReflogIntent::Skip)
			.await
			.expect("releasing the checkout lock lets the move through");
		assert_eq!(store.resolve("refs/heads/main").await.unwrap(), Some(next));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn head_transaction_publishes_only_to_the_captured_branch() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		store
			.update_ref("refs/heads/main", old, None, ReflogIntent::Skip)
			.await
			.unwrap();
		store
			.update_ref("refs/heads/other", old, None, ReflogIntent::Skip)
			.await
			.unwrap();
		store
			.set_symbolic("HEAD", "refs/heads/main", ReflogIntent::Skip)
			.await
			.unwrap();

		let mut transaction = store.lock_head_transaction().await.unwrap();
		assert_eq!(
			transaction.state(),
			&HeadState::Symbolic("refs/heads/main".to_owned())
		);
		assert_eq!(transaction.tip(), Some(old));
		let error = store
			.set_symbolic("HEAD", "refs/heads/other", ReflogIntent::Skip)
			.await
			.expect_err("the retained HEAD lock must reject a concurrent branch switch");
		assert!(
			matches!(&error, crate::RepositoryError::RefLocked { name } if name == "HEAD"),
			"expected HEAD.lock contention, got {error:?}",
		);

		transaction
			.prepare_reset(target, ReflogIntent::Skip)
			.await
			.unwrap();
		transaction.finish().await.unwrap();

		assert_eq!(
			store.resolve("refs/heads/main").await.unwrap(),
			Some(target)
		);
		assert_eq!(store.resolve("refs/heads/other").await.unwrap(), Some(old));
		assert_eq!(
			store.read_head().await.unwrap(),
			HeadState::Symbolic("refs/heads/main".to_owned())
		);
		assert_eq!(store.resolve("ORIG_HEAD").await.unwrap(), Some(old));
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn head_transaction_preserves_and_logs_a_symbolic_chain() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		let committer = "A U Thor <a@example.com> 1700000000 +0000";
		files
			.write_path_if_absent("HEAD", b"ref: refs/heads/alias\n")
			.await
			.unwrap();
		files
			.write_path_if_absent("refs/heads/alias", b"ref: refs/heads/main\n")
			.await
			.unwrap();
		files
			.write_path_if_absent("refs/heads/main", format!("{old}\n").as_bytes())
			.await
			.unwrap();

		let mut transaction = store.lock_head_transaction().await.unwrap();
		assert_eq!(transaction.tip(), Some(old));
		transaction
			.prepare_reset(
				target,
				ReflogIntent::Log {
					committer,
					message: "merge target: Fast-forward",
				},
			)
			.await
			.unwrap();
		transaction.finish().await.unwrap();

		assert_eq!(
			files.read_path("HEAD").await.unwrap(),
			b"ref: refs/heads/alias\n"
		);
		assert_eq!(
			files.read_path("refs/heads/alias").await.unwrap(),
			b"ref: refs/heads/main\n"
		);
		assert_eq!(
			store.resolve("refs/heads/main").await.unwrap(),
			Some(target)
		);
		assert_eq!(store.resolve("ORIG_HEAD").await.unwrap(), Some(old));
		let reflog = format!("{old} {target} {committer}\tmerge target: Fast-forward\n");
		for name in ["HEAD", "refs/heads/alias", "refs/heads/main"] {
			assert_eq!(
				files.read_path(&format!("logs/{name}")).await.unwrap(),
				reflog.as_bytes(),
				"the reset must log {name}"
			);
		}
	}

	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn head_transaction_deduplicates_orig_head_as_the_terminal_ref() {
		let files = MemoryFileStore::new();
		let store: RefStore<'_, MemoryFileStore, Sha256> = RefStore::new(&files);
		let old = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"old");
		let target = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"target");
		files
			.write_path_if_absent("HEAD", b"ref: refs/heads/alias\n")
			.await
			.unwrap();
		files
			.write_path_if_absent("refs/heads/alias", b"ref: ORIG_HEAD\n")
			.await
			.unwrap();
		files
			.write_path_if_absent("ORIG_HEAD", format!("{old}\n").as_bytes())
			.await
			.unwrap();

		let mut transaction = store.lock_head_transaction().await.unwrap();
		assert_eq!(transaction.tip(), Some(old));
		transaction
			.prepare_reset(target, ReflogIntent::Skip)
			.await
			.unwrap();
		transaction.finish().await.unwrap();

		assert_eq!(
			files.read_path("HEAD").await.unwrap(),
			b"ref: refs/heads/alias\n"
		);
		assert_eq!(
			files.read_path("refs/heads/alias").await.unwrap(),
			b"ref: ORIG_HEAD\n"
		);
		assert_eq!(store.resolve("ORIG_HEAD").await.unwrap(), Some(target));
	}

	/// Deleting a nested ref frees its parent name while keeping the namespace anchors: after
	/// `refs/heads/foo/bar` is deleted, the emptied `refs/heads/foo` is pruned but `refs/heads` and
	/// `refs/` survive (the repository's `refs/` must remain for `is_git_dir`). Guards the anchor bound in
	/// [`prune_empty_dirs`](super::RefStore::prune_empty_dirs).
	#[cfg(not(target_arch = "wasm32"))]
	#[tokio::test]
	async fn deleting_a_nested_ref_frees_the_parent_name_but_keeps_anchors() {
		use gitana_file_store_local::LocalFileStore;

		let tmp =
			std::env::temp_dir().join(format!("gitana-headlock-delnested-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&tmp);
		std::fs::create_dir_all(tmp.join("refs").join("heads")).unwrap();
		let files = LocalFileStore::from_dir(
			cap_std::fs::Dir::open_ambient_dir(&tmp, cap_std::ambient_authority()).unwrap(),
		);
		let store: RefStore<'_, LocalFileStore, Sha256> = RefStore::new(&files);
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"tip");

		store
			.update_ref("refs/heads/foo/bar", tip, None, ReflogIntent::Skip)
			.await
			.unwrap();
		store
			.delete_ref("refs/heads/foo/bar", Some(tip), ReflogIntent::Skip)
			.await
			.unwrap();

		// Immediately after the delete: anchors intact, emptied parent pruned.
		assert!(
			tmp.join("refs").is_dir(),
			"the refs/ anchor survives the delete"
		);
		assert!(
			tmp.join("refs").join("heads").is_dir(),
			"the refs/heads anchor survives the delete",
		);
		assert!(
			!tmp.join("refs").join("heads").join("foo").exists(),
			"the emptied refs/heads/foo is pruned so its name is free",
		);

		store
			.update_ref("refs/heads/foo", tip, None, ReflogIntent::Skip)
			.await
			.expect("refs/heads/foo is free after the nested ref was deleted");
		assert_eq!(store.resolve("refs/heads/foo").await.unwrap(), Some(tip));

		let _ = std::fs::remove_dir_all(&tmp);
	}
}
