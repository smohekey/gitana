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

/// With a module, its own context ("deinitializing submodule '<m>'"), the inner error following as its
/// [`source`](std::error::Error::source), as chain printers show it. Without one, the inner error's
/// message, the wrapper passing on the inner error's own source: each message once in the chain.
impl fmt::Display for DeinitFailure {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match &self.module {
			Some(module) => write!(formatter, "deinitializing submodule '{module}'"),
			None => fmt::Display::fmt(&self.source, formatter),
		}
	}
}

impl std::error::Error for DeinitFailure {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self.module {
			Some(_) => Some(&self.source),
			None => std::error::Error::source(&self.source),
		}
	}
}
