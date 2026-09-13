use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cap_std::{ambient_authority, fs::Dir};
use gitana_fs_native::{EntryIdentity, directory_identity};

/// The exact command directory opened before repository-serialization waits.
///
/// The native path remains available for Git-compatible path interpretation and diagnostics, while
/// filesystem consumers use the retained directory capability. Dispatch reopens the visible path
/// through the checked repository capabilities after every wait and compares it with `identity`
/// before handing this authority to a command.
pub(crate) struct RetainedCommandDirectory {
	path: PathBuf,
	directory: Dir,
	identity: EntryIdentity,
}

impl RetainedCommandDirectory {
	pub(crate) async fn capture(path: PathBuf) -> Result<Self> {
		blocking(move || {
			let directory = Dir::open_ambient_dir(&path, ambient_authority())
				.with_context(|| format!("retaining command working directory {}", path.display()))?;
			let identity = directory_identity(&directory)
				.with_context(|| format!("identifying command working directory {}", path.display()))?;
			Ok(Self {
				path,
				directory,
				identity,
			})
		})
		.await
	}

	pub(crate) fn path(&self) -> &Path {
		&self.path
	}

	pub(crate) fn into_parts(self) -> (PathBuf, Dir, EntryIdentity) {
		(self.path, self.directory, self.identity)
	}
}

/// Run retained-directory filesystem work without blocking the async runtime.
async fn blocking<T, F>(operation: F) -> Result<T>
where
	T: Send + 'static,
	F: FnOnce() -> Result<T> + Send + 'static,
{
	tokio::task::spawn_blocking(operation)
		.await
		.context("joining retained command-directory worker")?
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::{Arc, Condvar, Mutex};
	use std::time::Duration;

	use super::blocking;

	#[tokio::test(flavor = "current_thread")]
	async fn retained_directory_work_does_not_block_the_current_thread_runtime() {
		let entered = Arc::new(AtomicBool::new(false));
		let runtime_progressed = Arc::new(AtomicBool::new(false));
		let release = Arc::new((Mutex::new(false), Condvar::new()));
		let releaser = {
			let entered = Arc::clone(&entered);
			let release = Arc::clone(&release);
			std::thread::spawn(move || {
				while !entered.load(Ordering::SeqCst) {
					std::thread::yield_now();
				}
				std::thread::sleep(Duration::from_millis(50));
				let (lock, ready) = &*release;
				*lock.lock().unwrap() = true;
				ready.notify_one();
			})
		};
		let operation = blocking({
			let entered = Arc::clone(&entered);
			let runtime_progressed = Arc::clone(&runtime_progressed);
			let release = Arc::clone(&release);
			move || {
				entered.store(true, Ordering::SeqCst);
				let (lock, ready) = &*release;
				let mut released = lock.lock().unwrap();
				while !*released {
					released = ready.wait(released).unwrap();
				}
				if !runtime_progressed.load(Ordering::SeqCst) {
					anyhow::bail!("retained-directory work blocked the current-thread runtime");
				}
				Ok(())
			}
		});
		let heartbeat = async {
			while !entered.load(Ordering::SeqCst) {
				tokio::task::yield_now().await;
			}
			runtime_progressed.store(true, Ordering::SeqCst);
		};

		let (result, ()) = tokio::join!(operation, heartbeat);
		result.unwrap();
		releaser.join().unwrap();
	}
}
