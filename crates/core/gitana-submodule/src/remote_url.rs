use crate::SubmoduleError;

/// Select the first surviving fetch URL for `remote`, matching Git's multi-valued URL semantics.
/// An empty value clears earlier candidates, while a valueless key is malformed.
pub(crate) fn first_fetch_url<'a>(
	config: &'a gitana_config::GitConfig,
	remote: &str,
) -> Result<Option<&'a str>, SubmoduleError> {
	let mut selected = None;
	for (candidate, value) in config.variables_named("remote", "url") {
		if candidate != Some(remote) {
			continue;
		}
		match value {
			None => {
				return Err(SubmoduleError::MissingValue(format!("remote.{remote}.url")));
			}
			Some("") => selected = None,
			Some(url) if selected.is_none() => selected = Some(url),
			Some(_) => {}
		}
	}
	Ok(selected)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn selects_the_first_url_after_the_last_empty_reset() {
		let multiple =
			gitana_config::GitConfig::parse("[remote \"origin\"]\n\turl = first\n\turl = second\n")
				.unwrap();
		assert_eq!(first_fetch_url(&multiple, "origin").unwrap(), Some("first"));

		let reset = gitana_config::GitConfig::parse(
			"[remote \"origin\"]\n\turl = stale\n\turl =\n\turl = live\n\turl = ignored\n",
		)
		.unwrap();
		assert_eq!(first_fetch_url(&reset, "origin").unwrap(), Some("live"));
	}

	#[test]
	fn empty_resets_to_missing_and_valueless_is_rejected() {
		let reset =
			gitana_config::GitConfig::parse("[remote \"origin\"]\n\turl = stale\n\turl =\n").unwrap();
		assert_eq!(first_fetch_url(&reset, "origin").unwrap(), None);

		let valueless =
			gitana_config::GitConfig::parse("[remote \"origin\"]\n\turl = good\n\turl\n").unwrap();
		assert!(
			first_fetch_url(&valueless, "origin")
				.unwrap_err()
				.to_string()
				.contains("missing value")
		);
	}
}
