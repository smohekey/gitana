use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use cap_std::fs::Dir;
#[cfg(not(target_arch = "wasm32"))]
use gitana_fs_native::{EntryIdentity, directory_identity};

#[cfg(not(target_arch = "wasm32"))]
use crate::SharedConfigGuard;
use crate::SubmoduleError;

/// An opaque keepalive for one held submodule mutation lock.
///
/// Configuration providers must move this lease into any detached or blocking worker that can
/// continue mutating repository namespaces after its awaiting future is dropped.
#[derive(Clone)]
pub struct SubmoduleMutationLease {
	_owner: Arc<dyn Send + Sync>,
	#[cfg(not(target_arch = "wasm32"))]
	config_directories: Arc<Vec<EntryIdentity>>,
	#[cfg(not(target_arch = "wasm32"))]
	config_guards: Arc<Vec<Arc<SharedConfigGuard>>>,
}

impl SubmoduleMutationLease {
	/// Retain a serialization owner through a detached configuration mutation.
	pub fn retain<T>(owner: Arc<T>) -> Self
	where
		T: Send + Sync + 'static,
	{
		Self {
			_owner: owner,
			#[cfg(not(target_arch = "wasm32"))]
			config_directories: Arc::new(Vec::new()),
			#[cfg(not(target_arch = "wasm32"))]
			config_guards: Arc::new(Vec::new()),
		}
	}

	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn retain_config_directory<T>(
		owner: Arc<T>,
		identity: EntryIdentity,
		guard: Option<Arc<SharedConfigGuard>>,
	) -> Self
	where
		T: Send + Sync + 'static,
	{
		Self {
			_owner: owner,
			config_directories: Arc::new(vec![identity]),
			config_guards: Arc::new(guard.into_iter().collect()),
		}
	}

	/// Revalidate every visible common-config guard retained by this lease.
	#[cfg(not(target_arch = "wasm32"))]
	pub fn validate(&self) -> Result<(), SubmoduleError> {
		for guard in self.config_guards.iter() {
			guard.validate()?;
		}
		Ok(())
	}

	/// WASI leases carry cancellation keepalives but no native namespace guard.
	#[cfg(target_arch = "wasm32")]
	pub fn validate(&self) -> Result<(), SubmoduleError> {
		Ok(())
	}

	/// Report whether this lease already retains serialization for `directory`.
	///
	/// Local transports use this to avoid recursively acquiring a shared setup lock when their
	/// exact-root source aliases a repository whose mutation lock is already retained by the update.
	#[cfg(not(target_arch = "wasm32"))]
	pub fn covers_config_directory(&self, directory: &Dir) -> std::io::Result<bool> {
		let identity = directory_identity(directory)?;
		Ok(self.config_directories.contains(&identity))
	}

	/// Retain two independently acquired mutation authorities as one worker keepalive.
	#[cfg(not(target_arch = "wasm32"))]
	pub fn combine(self, other: Self) -> Self {
		let config_directories = {
			let mut config_directories = self.config_directories.as_ref().clone();
			for identity in other.config_directories.iter().copied() {
				if !config_directories.contains(&identity) {
					config_directories.push(identity);
				}
			}
			Arc::new(config_directories)
		};
		let config_guards = {
			let mut config_guards = self.config_guards.as_ref().clone();
			for guard in other.config_guards.iter() {
				if !config_guards
					.iter()
					.any(|current| Arc::ptr_eq(current, guard))
				{
					config_guards.push(Arc::clone(guard));
				}
			}
			Arc::new(config_guards)
		};
		Self {
			_owner: Arc::new((self, other)),
			config_directories,
			config_guards,
		}
	}
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
	use super::*;

	#[test]
	fn combined_lease_retains_both_owners() {
		let first = Arc::new(());
		let second = Arc::new(());
		let first_weak = Arc::downgrade(&first);
		let second_weak = Arc::downgrade(&second);
		let combined = SubmoduleMutationLease::retain(Arc::clone(&first))
			.combine(SubmoduleMutationLease::retain(Arc::clone(&second)));
		drop(first);
		drop(second);

		assert!(first_weak.upgrade().is_some());
		assert!(second_weak.upgrade().is_some());
		drop(combined);
		assert!(first_weak.upgrade().is_none());
		assert!(second_weak.upgrade().is_none());
	}
}
