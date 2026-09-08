use std::fmt;

/// Change one submodule declaration URL and synchronize registered configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct SetUrlRequest {
	/// Exact repository-root path from `.gitmodules`.
	pub path: String,
	/// New declaration URL, recorded verbatim after credential validation.
	pub url: String,
}

impl fmt::Debug for SetUrlRequest {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		let url = gitana_remote::redact_password(&self.url);
		formatter
			.debug_struct("SetUrlRequest")
			.field("path", &self.path)
			.field("url", &url)
			.finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn debug_redacts_the_url_password() {
		let request = SetUrlRequest {
			path: "modules/one".to_owned(),
			url: "https://alice:secret@example.test/repository".to_owned(),
		};

		for rendered in [format!("{request:?}"), format!("{request:#?}")] {
			assert!(!rendered.contains("secret"));
			assert!(rendered.contains("modules/one"));
			assert!(rendered.contains("https://alice@example.test/repository"));
		}
	}
}
