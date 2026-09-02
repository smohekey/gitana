use std::fmt;

use super::DeinitReport;
use crate::SubmoduleError;

/// A deinit failure that preserves the successfully completed prefix.
#[derive(Debug)]
pub struct DeinitFailure {
	pub completed: DeinitReport,
	pub module: Option<String>,
	pub source: SubmoduleError,
}

impl DeinitFailure {
	#[cfg(not(target_arch = "wasm32"))]
	pub(crate) fn preflight(source: SubmoduleError) -> Self {
		Self {
			completed: DeinitReport::default(),
			module: None,
			source,
		}
	}
}

impl fmt::Display for DeinitFailure {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		if let Some(module) = &self.module {
			write!(
				formatter,
				"deinitializing submodule '{module}': {}",
				self.source
			)
		} else {
			fmt::Display::fmt(&self.source, formatter)
		}
	}
}

impl std::error::Error for DeinitFailure {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}
