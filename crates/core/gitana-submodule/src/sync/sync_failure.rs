use std::fmt;

use super::SyncReport;
use crate::SubmoduleError;

/// A sync failure that preserves the successfully completed prefix.
#[derive(Debug)]
pub struct SyncFailure {
	pub completed: SyncReport,
	pub module: Option<String>,
	pub source: SubmoduleError,
}

impl SyncFailure {
	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn preflight(source: SubmoduleError) -> Self {
		Self {
			completed: SyncReport::default(),
			module: None,
			source,
		}
	}
}

impl fmt::Display for SyncFailure {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		if let Some(module) = &self.module {
			write!(
				formatter,
				"synchronizing submodule '{module}': {}",
				self.source
			)
		} else {
			fmt::Display::fmt(&self.source, formatter)
		}
	}
}

impl std::error::Error for SyncFailure {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}
