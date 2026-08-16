//! Targeted repository durability boundaries for externally published revisions and ref deletion.

use std::collections::{BTreeSet, HashMap, HashSet};

use gitana_file_store::{DeleteOutcome, DurabilityTarget, FileStore, FileStoreError};
use gitana_object::{HashAlgorithm, ObjectId, ObjectKind, parse_commit, referenced_ids};
use gitana_object_store::{ObjectBacking, ObjectStoreError};

use crate::{Config, HeadState, RefStore, Repository, RepositoryError};

const STABILITY_ATTEMPTS: usize = 4;

#[derive(Debug, PartialEq, Eq)]
struct ReachabilitySnapshot<H: HashAlgorithm> {
	objects: HashMap<ObjectId<H>, ObjectBacking>,
	shallow: HashSet<ObjectId<H>>,
	shallow_file: bool,
}

impl<F, H> Repository<F, H>
where
	F: FileStore,
	H: HashAlgorithm,
{
	/// Flush the repository metadata written by [`Repository::init`] before any ref is published.
	///
	/// Both files are parsed before the barrier and compared byte-for-byte afterward. Concurrent
	/// metadata movement is retried only while it converges to a stable view.
	pub async fn durability_barrier_initialized(&self) -> Result<(), RepositoryError> {
		let files = self.objects().file_store();
		for _ in 0..STABILITY_ATTEMPTS {
			let config = files.read_path("config").await?;
			let head = files.read_path("HEAD").await?;
			let parsed = Config::parse_bytes(&config)?;
			if parsed.object_format != H::NAME {
				return Err(RepositoryError::UnsupportedFormat(format!(
					"objectformat = {} does not match repository algorithm {}",
					parsed.object_format,
					H::NAME
				)));
			}
			HeadState::<H>::parse(&head)?;
			let targets = [
				DurabilityTarget::file("config"),
				DurabilityTarget::file("HEAD"),
			];
			if let Err(error) = files.durability_barrier(&targets).await {
				if files.read_path("config").await? == config && files.read_path("HEAD").await? == head {
					return Err(error.into());
				}
				continue;
			}
			if files.read_path("config").await? == config && files.read_path("HEAD").await? == head {
				return Ok(());
			}
		}

		Err(RepositoryError::DurabilityUnstable {
			name: "repository initialization".to_owned(),
		})
	}

	/// Flush every object reachable from `root` beyond the supplied durable frontiers without
	/// requiring a ref to name `root` yet.
	///
	/// This is the pre-publication half of a durable ref update: callers cross this barrier first,
	/// publish the ref with compare-and-swap, then finish with [`Self::durability_barrier_ref`]. The
	/// walk honours shallow boundaries and flushes the actual loose object or pack and index that
	/// supplied each object. Physical provenance is re-read after the barrier so a concurrent repack
	/// can only succeed after converging to a stable view.
	pub async fn durability_barrier_object_graph(
		&self,
		root: ObjectId<H>,
		durable_frontiers: &[ObjectId<H>],
	) -> Result<(), RepositoryError> {
		let frontiers: HashSet<_> = durable_frontiers.iter().copied().collect();
		let mut last_provenance_error = None;
		for _ in 0..STABILITY_ATTEMPTS {
			let reachability = match self.reachability_snapshot(root, &frontiers).await {
				Ok(snapshot) => {
					last_provenance_error = None;
					snapshot
				}
				Err(error) if is_provenance_movement(&error) => {
					last_provenance_error = Some(error);
					continue;
				}
				Err(error) => return Err(error),
			};
			let targets = durability_targets(&reachability, &BTreeSet::new());

			if let Err(error) = self
				.objects()
				.file_store()
				.durability_barrier(&targets)
				.await
			{
				let after = match self.reachability_snapshot(root, &frontiers).await {
					Ok(snapshot) => snapshot,
					Err(error) if is_provenance_movement(&error) => {
						last_provenance_error = Some(error);
						continue;
					}
					Err(error) => return Err(error),
				};
				if reachability == after {
					return Err(error.into());
				}
				continue;
			}

			let after = match self.reachability_snapshot(root, &frontiers).await {
				Ok(snapshot) => snapshot,
				Err(error) if is_provenance_movement(&error) => {
					last_provenance_error = Some(error);
					continue;
				}
				Err(error) => return Err(error),
			};
			if reachability == after {
				return Ok(());
			}
		}

		match last_provenance_error {
			Some(error) => Err(error),
			None => Err(RepositoryError::DurabilityUnstable {
				name: format!("object graph {root}"),
			}),
		}
	}

	/// Flush a named ref and every object newly reachable from `expected` beyond the supplied durable
	/// frontiers.
	///
	/// A frontier is trusted as an already-durable object boundary: the walk does not read or flush that
	/// object or anything reachable through it. This is the efficient shape for a session branch whose
	/// immutable baseline and previous checkpoints have already crossed a durability boundary. The walk
	/// honours `.git/shallow`, flushes the actual loose object or pack and existing index that supplied each
	/// object, and re-reads both the ref and object provenance after the barrier. Concurrent storage
	/// movement is retried only while it converges to a stable view.
	pub async fn durability_barrier_ref(
		&self,
		name: &str,
		expected: ObjectId<H>,
		durable_frontiers: &[ObjectId<H>],
	) -> Result<(), RepositoryError> {
		let frontiers: HashSet<_> = durable_frontiers.iter().copied().collect();
		let mut last_provenance_error = None;
		for _ in 0..STABILITY_ATTEMPTS {
			self.require_ref(name, Some(expected)).await?;
			let reachability = match self.reachability_snapshot(expected, &frontiers).await {
				Ok(snapshot) => {
					last_provenance_error = None;
					snapshot
				}
				Err(error) if is_provenance_movement(&error) => {
					last_provenance_error = Some(error);
					continue;
				}
				Err(error) => return Err(error),
			};
			let ref_files = self.ref_files(name).await?;
			let targets = durability_targets(&reachability, &ref_files);

			if let Err(error) = self
				.objects()
				.file_store()
				.durability_barrier(&targets)
				.await
			{
				self.require_ref(name, Some(expected)).await?;
				let after = match self.reachability_snapshot(expected, &frontiers).await {
					Ok(snapshot) => snapshot,
					Err(error) if is_provenance_movement(&error) => {
						last_provenance_error = Some(error);
						continue;
					}
					Err(error) => return Err(error),
				};
				let after_ref_files = self.ref_files(name).await?;
				if reachability == after && ref_files == after_ref_files {
					return Err(error.into());
				}
				continue;
			}

			self.require_ref(name, Some(expected)).await?;
			let after = match self.reachability_snapshot(expected, &frontiers).await {
				Ok(snapshot) => snapshot,
				Err(error) if is_provenance_movement(&error) => {
					last_provenance_error = Some(error);
					continue;
				}
				Err(error) => return Err(error),
			};
			let after_ref_files = self.ref_files(name).await?;
			if reachability == after && ref_files == after_ref_files {
				return Ok(());
			}
		}

		match last_provenance_error {
			Some(error) => Err(error),
			None => Err(RepositoryError::DurabilityUnstable {
				name: name.to_owned(),
			}),
		}
	}

	/// Flush the namespaces that prove `name` remains absent after a ref deletion.
	///
	/// A stale loose reflog is removed before the barrier because ref deletion treats reflog cleanup as
	/// best-effort. The method then flushes `packed-refs` when present and the closest surviving ref/log
	/// namespace directories, rechecking absence after the barrier. It never removes a live ref.
	pub async fn durability_barrier_ref_absent(&self, name: &str) -> Result<(), RepositoryError> {
		let files = self.objects().file_store().shared_handle();
		let name = name.to_owned();
		#[cfg(not(target_arch = "wasm32"))]
		{
			match tokio::spawn(async move {
				durability_barrier_ref_absent_inline::<_, H>(&files, &name).await
			})
			.await
			{
				Ok(result) => result,
				Err(error) => Err(RepositoryError::RetainedTask(error.to_string())),
			}
		}
		#[cfg(target_arch = "wasm32")]
		{
			durability_barrier_ref_absent_inline::<_, H>(&files, &name).await
		}
	}

	async fn require_ref(
		&self,
		name: &str,
		expected: Option<ObjectId<H>>,
	) -> Result<(), RepositoryError> {
		let actual = self.refs().resolve(name).await?;
		if actual == expected {
			Ok(())
		} else {
			Err(RepositoryError::RefMoved {
				name: name.to_owned(),
			})
		}
	}

	async fn reachability_snapshot(
		&self,
		root: ObjectId<H>,
		frontiers: &HashSet<ObjectId<H>>,
	) -> Result<ReachabilitySnapshot<H>, RepositoryError> {
		let shallow: HashSet<_> = self.read_shallow().await?.into_iter().collect();
		let shallow_file = self.objects().file_store().exists("shallow").await?;
		let mut objects = HashMap::new();
		let mut seen = HashSet::new();
		let mut pending = vec![root];
		let mut reader = self.objects().read_session();

		while let Some(id) = pending.pop() {
			if frontiers.contains(&id) || !seen.insert(id) {
				continue;
			}
			let (kind, data, backing) = reader.read_object_with_backing(&id).await?;
			objects.insert(id, backing);
			if kind == ObjectKind::Commit && shallow.contains(&id) {
				pending.push(parse_commit::<H>(&data)?.tree);
			} else {
				pending.extend(referenced_ids::<H>(kind, &data)?);
			}
		}

		Ok(ReachabilitySnapshot {
			objects,
			shallow,
			shallow_file,
		})
	}

	async fn ref_files(&self, name: &str) -> Result<BTreeSet<String>, RepositoryError> {
		let files = self.objects().file_store();
		let mut paths = BTreeSet::new();
		if files.exists(name).await? {
			paths.insert(name.to_owned());
		} else if files.exists("packed-refs").await? {
			paths.insert("packed-refs".to_owned());
		}
		let reflog = format!("logs/{name}");
		if files.exists(&reflog).await? {
			paths.insert(reflog);
		}
		Ok(paths)
	}
}

async fn durability_barrier_ref_absent_inline<F, H>(
	files: &F,
	name: &str,
) -> Result<(), RepositoryError>
where
	F: FileStore,
	H: HashAlgorithm,
{
	let refs = RefStore::<F, H>::new(files);
	let reflog = format!("logs/{name}");
	for _ in 0..STABILITY_ATTEMPTS {
		let lock = refs.lock_ref(name).await?;
		let resolved = refs.resolve(name).await;
		if matches!(resolved, Ok(Some(_))) {
			drop(lock);
			refs.prune_empty_dirs(name).await;
			return Err(RepositoryError::RefMoved {
				name: name.to_owned(),
			});
		}
		if let Err(error) = resolved {
			drop(lock);
			refs.prune_empty_dirs(name).await;
			return Err(error);
		}
		let deleted = files.delete_path(&reflog, None).await;
		drop(lock);
		refs.prune_empty_dirs(name).await;
		match deleted? {
			DeleteOutcome::Deleted | DeleteOutcome::NotFound => {}
		}

		let targets = absent_ref_targets(files, name).await?;
		if let Err(error) = files.durability_barrier(&targets).await {
			if refs.resolve(name).await?.is_none()
				&& !files.exists(&reflog).await?
				&& targets == absent_ref_targets(files, name).await?
			{
				return Err(error.into());
			}
			continue;
		}
		if refs.resolve(name).await?.is_none()
			&& !files.exists(&reflog).await?
			&& targets == absent_ref_targets(files, name).await?
		{
			return Ok(());
		}
	}

	Err(RepositoryError::DurabilityUnstable {
		name: name.to_owned(),
	})
}

async fn absent_ref_targets(
	files: &impl FileStore,
	name: &str,
) -> Result<Vec<DurabilityTarget>, RepositoryError> {
	let mut targets = Vec::new();
	if files.exists("packed-refs").await? {
		targets.push(DurabilityTarget::file("packed-refs"));
	}
	targets.push(DurabilityTarget::directory(
		closest_existing_directory(files, parent_of(name).unwrap_or("")).await?,
	));
	let reflog = format!("logs/{name}");
	targets.push(DurabilityTarget::directory(
		closest_existing_directory(files, parent_of(&reflog).unwrap_or("")).await?,
	));
	Ok(targets)
}

fn is_provenance_movement(error: &RepositoryError) -> bool {
	matches!(
		error,
		RepositoryError::ObjectStore(ObjectStoreError::NotFound)
			| RepositoryError::ObjectStore(ObjectStoreError::FileStore(FileStoreError::NotFound))
			| RepositoryError::FileStore(FileStoreError::NotFound)
	)
}

fn durability_targets<H: HashAlgorithm>(
	reachability: &ReachabilitySnapshot<H>,
	ref_files: &BTreeSet<String>,
) -> Vec<DurabilityTarget> {
	let mut files = ref_files.clone();
	if reachability.shallow_file {
		files.insert("shallow".to_owned());
	}
	for backing in reachability.objects.values() {
		files.extend(backing.files().map(str::to_owned));
	}
	files.into_iter().map(DurabilityTarget::file).collect()
}

async fn closest_existing_directory(
	files: &impl FileStore,
	path: &str,
) -> Result<String, RepositoryError> {
	let mut current = path;
	loop {
		if current.is_empty() || files.is_dir(current).await? {
			return Ok(current.to_owned());
		}
		current = parent_of(current).unwrap_or("");
	}
}

fn parent_of(path: &str) -> Option<&str> {
	path.rfind('/').map(|index| &path[..index])
}

#[cfg(test)]
mod tests {
	use std::future::Future;
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
	use std::sync::{Arc, Mutex};

	use gitana_file_store::{ByteReader, PathLock, Result as FileResult, Version, WriteOutcome};
	use gitana_file_store_memory::MemoryFileStore;
	use gitana_object::{
		Commit, ObjectKind, PackedObject, Sha256, TreeEntry, encode_commit, encode_pack, encode_tree,
		loose_object_path,
	};
	use gitana_object_store::ObjectStore;
	use tokio::sync::Notify;

	use super::*;
	use crate::{FileMode, ReflogIntent, TreeBuildEntry};

	#[derive(Default)]
	struct OperationGate {
		armed: AtomicBool,
		entered: AtomicBool,
		released: AtomicBool,
		entered_notify: Notify,
		release_notify: Notify,
	}

	impl OperationGate {
		fn arm(&self) {
			self.entered.store(false, Ordering::Release);
			self.released.store(false, Ordering::Release);
			self.armed.store(true, Ordering::Release);
		}

		async fn pause_if_armed(&self) {
			if !self.armed.swap(false, Ordering::AcqRel) {
				return;
			}
			self.entered.store(true, Ordering::Release);
			self.entered_notify.notify_waiters();
			while !self.released.load(Ordering::Acquire) {
				self.release_notify.notified().await;
			}
		}

		async fn wait_until_entered(&self) {
			while !self.entered.load(Ordering::Acquire) {
				self.entered_notify.notified().await;
			}
		}

		fn release(&self) {
			self.released.store(true, Ordering::Release);
			self.release_notify.notify_waiters();
		}
	}

	struct RecordingFileStore {
		inner: Arc<MemoryFileStore>,
		barriers: Arc<Mutex<Vec<Vec<DurabilityTarget>>>>,
		move_loose_on_barrier: Arc<AtomicBool>,
		index_reads: Arc<AtomicUsize>,
		reflog_delete_gate: Arc<OperationGate>,
		barrier_gate: Arc<OperationGate>,
	}

	impl RecordingFileStore {
		fn new() -> (Self, Arc<Mutex<Vec<Vec<DurabilityTarget>>>>) {
			let barriers = Arc::new(Mutex::new(Vec::new()));
			(
				Self {
					inner: Arc::new(MemoryFileStore::new()),
					barriers: Arc::clone(&barriers),
					move_loose_on_barrier: Arc::new(AtomicBool::new(false)),
					index_reads: Arc::new(AtomicUsize::new(0)),
					reflog_delete_gate: Arc::new(OperationGate::default()),
					barrier_gate: Arc::new(OperationGate::default()),
				},
				barriers,
			)
		}

		fn move_loose_on_next_barrier(&self) {
			self.move_loose_on_barrier.store(true, Ordering::Release);
		}

		fn reset_index_reads(&self) {
			self.index_reads.store(0, Ordering::Release);
		}

		fn index_reads(&self) -> usize {
			self.index_reads.load(Ordering::Acquire)
		}
	}

	impl FileStore for RecordingFileStore {
		type Shared = Self;

		fn shared_handle(&self) -> Self::Shared {
			Self {
				inner: Arc::clone(&self.inner),
				barriers: Arc::clone(&self.barriers),
				move_loose_on_barrier: Arc::clone(&self.move_loose_on_barrier),
				index_reads: Arc::clone(&self.index_reads),
				reflog_delete_gate: Arc::clone(&self.reflog_delete_gate),
				barrier_gate: Arc::clone(&self.barrier_gate),
			}
		}

		async fn durability_barrier(&self, targets: &[DurabilityTarget]) -> FileResult<()> {
			self.barrier_gate.pause_if_armed().await;
			self.barriers.lock().unwrap().push(targets.to_vec());
			if self.move_loose_on_barrier.swap(false, Ordering::AcqRel) {
				for target in targets {
					if let DurabilityTarget::File(path) = target
						&& path.starts_with("objects/")
						&& !path.starts_with("objects/pack/")
					{
						self.inner.delete_path_unlocked(path).await?;
					}
				}
			}
			Ok(())
		}

		fn read_path(&self, path: &str) -> impl Future<Output = FileResult<Vec<u8>>> + Send {
			if path.ends_with(".idx") {
				self.index_reads.fetch_add(1, Ordering::AcqRel);
			}
			self.inner.read_path(path)
		}

		fn read_path_versioned(
			&self,
			path: &str,
		) -> impl Future<Output = FileResult<(Vec<u8>, Version)>> + Send {
			self.inner.read_path_versioned(path)
		}

		fn write_path_if_absent(
			&self,
			path: &str,
			bytes: &[u8],
		) -> impl Future<Output = FileResult<WriteOutcome>> + Send {
			self.inner.write_path_if_absent(path, bytes)
		}

		fn try_lock_path(
			&self,
			path: &str,
		) -> impl Future<Output = FileResult<Option<PathLock>>> + Send {
			self.inner.try_lock_path(path)
		}

		fn write_path_cas(
			&self,
			path: &str,
			bytes: &[u8],
			expected: Option<&Version>,
		) -> impl Future<Output = FileResult<Version>> + Send {
			self.inner.write_path_cas(path, bytes, expected)
		}

		fn write_path_replace(
			&self,
			path: &str,
			bytes: &[u8],
		) -> impl Future<Output = FileResult<()>> + Send {
			self.inner.write_path_replace(path, bytes)
		}

		fn delete_path(
			&self,
			path: &str,
			expected: Option<&Version>,
		) -> impl Future<Output = FileResult<DeleteOutcome>> + Send {
			let gate = Arc::clone(&self.reflog_delete_gate);
			async move {
				if path == "logs/refs/heads/session" {
					gate.pause_if_armed().await;
				}
				self.inner.delete_path(path, expected).await
			}
		}

		fn delete_path_unlocked(
			&self,
			path: &str,
		) -> impl Future<Output = FileResult<DeleteOutcome>> + Send {
			self.inner.delete_path_unlocked(path)
		}

		fn remove_dir(&self, path: &str) -> impl Future<Output = FileResult<()>> + Send {
			self.inner.remove_dir(path)
		}

		fn exists(&self, path: &str) -> impl Future<Output = FileResult<bool>> + Send {
			self.inner.exists(path)
		}

		fn is_dir(&self, path: &str) -> impl Future<Output = FileResult<bool>> + Send {
			self.inner.is_dir(path)
		}

		fn size(&self, path: &str) -> impl Future<Output = FileResult<u64>> + Send {
			self.inner.size(path)
		}

		fn list_prefix(&self, prefix: &str) -> impl Future<Output = FileResult<Vec<String>>> + Send {
			self.inner.list_prefix(prefix)
		}

		fn read_path_range(
			&self,
			path: &str,
			offset: u64,
			length: u64,
		) -> impl Future<Output = FileResult<Vec<u8>>> + Send {
			self.inner.read_path_range(path, offset, length)
		}

		fn read_path_stream(&self, path: &str) -> impl Future<Output = FileResult<ByteReader>> + Send {
			self.inner.read_path_stream(path)
		}

		fn write_path_stream_if_absent(
			&self,
			path: &str,
			reader: ByteReader,
			len: u64,
		) -> impl Future<Output = FileResult<WriteOutcome>> + Send {
			self.inner.write_path_stream_if_absent(path, reader, len)
		}

		fn remove_lock_file_sync(&self, path: &str) {
			self.inner.remove_lock_file_sync(path);
		}

		fn replace_and_release_lock(
			&self,
			path: &str,
			bytes: &[u8],
			lock_path: &str,
		) -> impl Future<Output = FileResult<()>> + Send {
			self.inner.replace_and_release_lock(path, bytes, lock_path)
		}
	}

	#[tokio::test]
	async fn initialization_barrier_flushes_repository_metadata() {
		let (files, barriers) = RecordingFileStore::new();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repo.init().await.unwrap();

		repo.durability_barrier_initialized().await.unwrap();

		let calls = barriers.lock().unwrap();
		assert_eq!(calls.len(), 1);
		assert_eq!(
			calls[0],
			vec![
				DurabilityTarget::file("config"),
				DurabilityTarget::file("HEAD")
			]
		);
	}

	#[tokio::test]
	async fn initialization_barrier_rejects_a_mismatched_object_format() {
		let (files, barriers) = RecordingFileStore::new();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repo.init().await.unwrap();
		repo
			.objects()
			.file_store()
			.write_path_replace("config", Config::sha1().render().as_bytes())
			.await
			.unwrap();

		assert!(matches!(
			repo.durability_barrier_initialized().await,
			Err(RepositoryError::UnsupportedFormat(reason))
				if reason.contains("does not match repository algorithm sha256")
		));
		assert!(barriers.lock().unwrap().is_empty());
	}

	#[tokio::test]
	async fn object_graph_barrier_does_not_require_a_published_ref() {
		let (files, barriers) = RecordingFileStore::new();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repo.init().await.unwrap();
		let baseline_blob = repo.write_blob(b"base").await.unwrap();
		let baseline_tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "value".to_owned(),
				mode: FileMode::Regular,
				id: baseline_blob,
			}])
			.await
			.unwrap();
		let baseline = repo
			.create_commit(
				baseline_tree,
				Vec::new(),
				"A <a@b> 1 +0000",
				"A <a@b> 1 +0000",
				"base",
			)
			.await
			.unwrap();
		let new_blob = repo.write_blob(b"new").await.unwrap();
		let new_tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "value".to_owned(),
				mode: FileMode::Regular,
				id: new_blob,
			}])
			.await
			.unwrap();
		let tip = repo
			.create_commit(
				new_tree,
				vec![baseline],
				"A <a@b> 2 +0000",
				"A <a@b> 2 +0000",
				"tip",
			)
			.await
			.unwrap();

		repo
			.durability_barrier_object_graph(tip, &[baseline])
			.await
			.unwrap();
		assert!(
			repo
				.refs()
				.resolve("refs/heads/session")
				.await
				.unwrap()
				.is_none()
		);

		let calls = barriers.lock().unwrap();
		assert_eq!(calls.len(), 1);
		let files: HashSet<_> = calls[0]
			.iter()
			.filter_map(|target| match target {
				DurabilityTarget::File(path) => Some(path.as_str()),
				DurabilityTarget::Directory(_) | DurabilityTarget::Tree(_) => None,
			})
			.collect();
		for expected in [tip, new_tree, new_blob] {
			assert!(files.contains(loose_object_path(&expected).as_str()));
		}
		for durable in [baseline, baseline_tree, baseline_blob] {
			assert!(!files.contains(loose_object_path(&durable).as_str()));
		}
	}

	#[tokio::test]
	async fn ref_barrier_flushes_only_objects_beyond_the_durable_frontier() {
		let (files, barriers) = RecordingFileStore::new();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repo.init().await.unwrap();

		let baseline_blob = repo.write_blob(b"baseline").await.unwrap();
		let baseline_tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "value".to_owned(),
				mode: FileMode::Regular,
				id: baseline_blob,
			}])
			.await
			.unwrap();
		let baseline = repo
			.create_commit(
				baseline_tree,
				Vec::new(),
				"A <a@b> 1 +0000",
				"A <a@b> 1 +0000",
				"base",
			)
			.await
			.unwrap();

		let new_blob = repo.write_blob(b"new").await.unwrap();
		let new_tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "value".to_owned(),
				mode: FileMode::Regular,
				id: new_blob,
			}])
			.await
			.unwrap();
		let tip = repo
			.create_commit(
				new_tree,
				vec![baseline],
				"A <a@b> 2 +0000",
				"A <a@b> 2 +0000",
				"tip",
			)
			.await
			.unwrap();
		repo
			.refs()
			.update_ref("refs/heads/session", tip, None, ReflogIntent::Skip)
			.await
			.unwrap();

		repo
			.durability_barrier_ref("refs/heads/session", tip, &[baseline])
			.await
			.unwrap();
		let calls = barriers.lock().unwrap();
		assert_eq!(calls.len(), 1);
		let files: HashSet<_> = calls[0]
			.iter()
			.filter_map(|target| match target {
				DurabilityTarget::File(path) => Some(path.as_str()),
				DurabilityTarget::Directory(_) | DurabilityTarget::Tree(_) => None,
			})
			.collect();
		for expected in [
			"refs/heads/session".to_owned(),
			loose_object_path(&tip),
			loose_object_path(&new_tree),
			loose_object_path(&new_blob),
		] {
			assert!(
				files.contains(expected.as_str()),
				"missing durability target {expected}"
			);
		}
		for durable in [baseline, baseline_tree, baseline_blob] {
			assert!(
				!files.contains(loose_object_path(&durable).as_str()),
				"durable frontier object must not be flushed again"
			);
		}
	}

	#[tokio::test]
	async fn ref_barrier_stops_commit_parent_walk_at_a_shallow_boundary() {
		let (files, barriers) = RecordingFileStore::new();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repo.init().await.unwrap();
		let blob = repo.write_blob(b"value").await.unwrap();
		let tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "value".to_owned(),
				mode: FileMode::Regular,
				id: blob,
			}])
			.await
			.unwrap();
		let absent_parent = ObjectId::<Sha256>::compute(ObjectKind::Commit, b"absent parent");
		let boundary = repo
			.create_commit(
				tree,
				vec![absent_parent],
				"A <a@b> 1 +0000",
				"A <a@b> 1 +0000",
				"boundary",
			)
			.await
			.unwrap();
		repo.write_shallow(&[boundary]).await.unwrap();
		repo
			.refs()
			.update_ref("refs/heads/session", boundary, None, ReflogIntent::Skip)
			.await
			.unwrap();

		repo
			.durability_barrier_ref("refs/heads/session", boundary, &[])
			.await
			.expect("the missing parent beyond the shallow boundary is not traversed");
		assert!(
			barriers.lock().unwrap()[0]
				.iter()
				.any(|target| matches!(target, DurabilityTarget::File(path) if path == "shallow"))
		);
	}

	#[tokio::test]
	async fn ref_barrier_flushes_the_pack_and_index_that_supply_reachable_objects() {
		let (files, barriers) = RecordingFileStore::new();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repo.init().await.unwrap();
		let blob_data = b"packed".to_vec();
		let blob = ObjectId::<Sha256>::compute(ObjectKind::Blob, &blob_data);
		let tree_data = encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: "value".to_owned(),
			id: blob,
		}]);
		let tree = ObjectId::<Sha256>::compute(ObjectKind::Tree, &tree_data);
		let commit_data = encode_commit(&Commit {
			tree,
			parents: Vec::new(),
			author: "A <a@b> 1 +0000".to_owned(),
			committer: "A <a@b> 1 +0000".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: "packed".to_owned(),
		});
		let tip = ObjectId::<Sha256>::compute(ObjectKind::Commit, &commit_data);
		repo
			.objects()
			.write_pack(encode_pack(&[
				PackedObject {
					id: blob,
					kind: ObjectKind::Blob,
					data: blob_data,
				},
				PackedObject {
					id: tree,
					kind: ObjectKind::Tree,
					data: tree_data,
				},
				PackedObject {
					id: tip,
					kind: ObjectKind::Commit,
					data: commit_data,
				},
			]))
			.await
			.unwrap();
		repo
			.refs()
			.update_ref("refs/heads/session", tip, None, ReflogIntent::Skip)
			.await
			.unwrap();
		repo.objects().file_store().reset_index_reads();

		repo
			.durability_barrier_ref("refs/heads/session", tip, &[])
			.await
			.unwrap();
		assert_eq!(
			repo.objects().file_store().index_reads(),
			3,
			"one pack index is loaded for discovery and once per provenance snapshot"
		);
		{
			let calls = barriers.lock().unwrap();
			let files: Vec<_> = calls[0]
				.iter()
				.filter_map(|target| match target {
					DurabilityTarget::File(path) => Some(path.as_str()),
					DurabilityTarget::Directory(_) | DurabilityTarget::Tree(_) => None,
				})
				.collect();
			assert!(files.iter().any(|path| path.ends_with(".pack")));
			assert!(files.iter().any(|path| path.ends_with(".idx")));
			assert!(!files.contains(&loose_object_path(&tip).as_str()));
		}

		let index = repo
			.objects()
			.file_store()
			.list_prefix("objects/pack/")
			.await
			.unwrap()
			.into_iter()
			.find(|path| path.ends_with(".idx"))
			.unwrap();
		repo
			.objects()
			.file_store()
			.delete_path(&index, None)
			.await
			.unwrap();
		barriers.lock().unwrap().clear();
		repo
			.durability_barrier_ref("refs/heads/session", tip, &[])
			.await
			.expect("a readable pack without a sidecar remains a valid durability source");
		let calls = barriers.lock().unwrap();
		let files: Vec<_> = calls[0]
			.iter()
			.filter_map(|target| match target {
				DurabilityTarget::File(path) => Some(path.as_str()),
				DurabilityTarget::Directory(_) | DurabilityTarget::Tree(_) => None,
			})
			.collect();
		assert!(files.iter().any(|path| path.ends_with(".pack")));
		assert!(!files.iter().any(|path| path.ends_with(".idx")));
	}

	#[tokio::test]
	async fn ordinary_pack_reads_do_not_reopen_the_index_for_backing_provenance() {
		let (files, _) = RecordingFileStore::new();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		let data = b"packed".to_vec();
		let id = ObjectId::<Sha256>::compute(ObjectKind::Blob, &data);
		repo
			.objects()
			.write_pack(encode_pack(&[PackedObject {
				id,
				kind: ObjectKind::Blob,
				data,
			}]))
			.await
			.unwrap();
		repo.objects().file_store().reset_index_reads();

		repo.objects().read_object(&id).await.unwrap();
		repo.objects().read_object(&id).await.unwrap();

		assert_eq!(repo.objects().file_store().index_reads(), 1);
	}

	#[tokio::test]
	async fn absent_ref_barrier_removes_a_stale_reflog_before_flushing_namespaces() {
		let (files, barriers) = RecordingFileStore::new();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repo.init().await.unwrap();
		let blob = repo.write_blob(b"value").await.unwrap();
		let tree = repo
			.write_tree(&[TreeBuildEntry {
				path: "value".to_owned(),
				mode: FileMode::Regular,
				id: blob,
			}])
			.await
			.unwrap();
		let tip = repo
			.create_commit(
				tree,
				Vec::new(),
				"A <a@b> 1 +0000",
				"A <a@b> 1 +0000",
				"tip",
			)
			.await
			.unwrap();
		repo
			.refs()
			.update_ref("refs/heads/session", tip, None, ReflogIntent::Skip)
			.await
			.unwrap();
		repo
			.refs()
			.delete_ref("refs/heads/session", Some(tip), ReflogIntent::Skip)
			.await
			.unwrap();
		repo
			.objects()
			.file_store()
			.write_path_if_absent("logs/refs/heads/session", b"stale")
			.await
			.unwrap();

		repo
			.durability_barrier_ref_absent("refs/heads/session")
			.await
			.unwrap();
		assert!(
			!repo
				.objects()
				.file_store()
				.exists("logs/refs/heads/session")
				.await
				.unwrap()
		);
		assert!(
			barriers.lock().unwrap()[0]
				.iter()
				.any(|target| matches!(target, DurabilityTarget::Directory(_)))
		);
	}

	#[tokio::test]
	async fn absent_ref_barrier_does_not_delete_a_racing_creators_reflog() {
		let (files, _) = RecordingFileStore::new();
		let control = files.shared_handle();
		let repo = Arc::new(Repository::<_, Sha256>::new(ObjectStore::new(files)));
		repo.init().await.unwrap();
		let tip = repo.write_blob(b"new tip").await.unwrap();
		repo
			.objects()
			.file_store()
			.write_path_if_absent("logs/refs/heads/session", b"stale")
			.await
			.unwrap();
		control.reflog_delete_gate.arm();
		control.barrier_gate.arm();

		let barrier_repo = Arc::clone(&repo);
		let barrier = tokio::spawn(async move {
			barrier_repo
				.durability_barrier_ref_absent("refs/heads/session")
				.await
		});
		control.reflog_delete_gate.wait_until_entered().await;

		let update_repo = Arc::clone(&repo);
		let update = tokio::spawn(async move {
			update_repo
				.refs()
				.update_ref(
					"refs/heads/session",
					tip,
					None,
					ReflogIntent::Log {
						committer: "A <a@b> 1 +0000",
						message: "racing create",
					},
				)
				.await
		});
		for _ in 0..4 {
			tokio::task::yield_now().await;
		}
		assert!(
			!update.is_finished(),
			"the ref creator must wait for stale-reflog cleanup"
		);

		control.reflog_delete_gate.release();
		control.barrier_gate.wait_until_entered().await;
		update.await.unwrap().unwrap();
		control.barrier_gate.release();

		assert!(matches!(
			barrier.await.unwrap(),
			Err(RepositoryError::RefMoved { name }) if name == "refs/heads/session"
		));
		let reflog = repo
			.objects()
			.file_store()
			.read_path("logs/refs/heads/session")
			.await
			.unwrap();
		assert!(String::from_utf8(reflog).unwrap().contains("racing create"));
	}

	#[tokio::test]
	async fn ref_barrier_retries_when_objects_move_from_loose_storage_to_a_pack() {
		let (files, barriers) = RecordingFileStore::new();
		let control = files.shared_handle();
		let repo = Repository::<_, Sha256>::new(ObjectStore::new(files));
		repo.init().await.unwrap();
		let blob_data = b"moving".to_vec();
		let blob = repo
			.objects()
			.write_object(ObjectKind::Blob, &blob_data)
			.await
			.unwrap();
		let tree_data = encode_tree(&[TreeEntry {
			mode: "100644".to_owned(),
			name: "value".to_owned(),
			id: blob,
		}]);
		let tree = repo
			.objects()
			.write_object(ObjectKind::Tree, &tree_data)
			.await
			.unwrap();
		let commit_data = encode_commit(&Commit {
			tree,
			parents: Vec::new(),
			author: "A <a@b> 1 +0000".to_owned(),
			committer: "A <a@b> 1 +0000".to_owned(),
			signature: None,
			extra_headers: Vec::new(),
			message: "moving".to_owned(),
		});
		let tip = repo
			.objects()
			.write_object(ObjectKind::Commit, &commit_data)
			.await
			.unwrap();
		repo
			.objects()
			.write_pack(encode_pack(&[
				PackedObject {
					id: blob,
					kind: ObjectKind::Blob,
					data: blob_data,
				},
				PackedObject {
					id: tree,
					kind: ObjectKind::Tree,
					data: tree_data,
				},
				PackedObject {
					id: tip,
					kind: ObjectKind::Commit,
					data: commit_data,
				},
			]))
			.await
			.unwrap();
		repo
			.refs()
			.update_ref("refs/heads/session", tip, None, ReflogIntent::Skip)
			.await
			.unwrap();
		control.move_loose_on_next_barrier();

		repo
			.durability_barrier_ref("refs/heads/session", tip, &[])
			.await
			.unwrap();
		let calls = barriers.lock().unwrap();
		assert_eq!(
			calls.len(),
			2,
			"provenance change requires a second barrier"
		);
		assert!(calls[0].iter().any(
			|target| matches!(target, DurabilityTarget::File(path) if path == &loose_object_path(&tip))
		));
		assert!(
			calls[1]
				.iter()
				.any(|target| matches!(target, DurabilityTarget::File(path) if path.ends_with(".pack")))
		);
		assert!(
			calls[1]
				.iter()
				.any(|target| matches!(target, DurabilityTarget::File(path) if path.ends_with(".idx")))
		);
	}
}
