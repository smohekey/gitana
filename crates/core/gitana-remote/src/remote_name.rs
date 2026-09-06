use anyhow::{Result, bail};

/// Validate a remote name used as the middle portion of `refs/remotes/<name>/<branch>`.
///
/// Whole-refname suffix rules do not apply to this middle portion, so Git accepts names such as
/// `backup.` and `@`. Each slash-separated component must still be safe inside a refname.
pub fn validate_remote_name(name: &str) -> Result<()> {
	let anywhere_bad = name.contains("..")
		|| name.contains("@{")
		|| name.bytes().any(|byte| {
			byte <= b' '
				|| byte == 0x7f
				|| matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
		});
	let component_bad = name.split('/').any(|component| {
		component.is_empty() || component.starts_with('.') || component.ends_with(".lock")
	});
	if name.is_empty() || anywhere_bad || component_bad {
		bail!("invalid remote name: '{name}'");
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::validate_remote_name;

	#[test]
	fn accepts_names_used_as_middle_ref_components() {
		for name in [
			"origin",
			"backup.",
			"@",
			"team/upstream",
			"non\u{a0}breaking",
		] {
			validate_remote_name(name).unwrap();
		}
	}

	#[test]
	fn rejects_unsafe_ref_components() {
		for name in [
			"", ".", "/origin", "origin/", "a//b", ".hidden", "a.lock", "a..b", "a@{b", "a:b",
		] {
			assert!(validate_remote_name(name).is_err(), "accepted {name:?}");
		}
	}
}
