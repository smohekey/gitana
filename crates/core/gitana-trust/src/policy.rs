use std::fmt;

/// The enforcement policy a trust root declares (see `docs/hlds/secure-git-trust-signing.md`).
/// Verification computes the same result regardless of policy; the policy tells the *enforcing*
/// layer (receive-pack) whether to reject on a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
	/// No trust enforcement.
	Off,
	/// Verify and record failures, but do not reject writes.
	Warn,
	/// Reject unsigned, untrusted, stale, malformed, or unverifiable protected writes.
	Require,
}

impl Policy {
	/// The canonical lowercase name (`off`/`warn`/`require`), matching the serialized document form.
	pub fn as_str(self) -> &'static str {
		match self {
			Policy::Off => "off",
			Policy::Warn => "warn",
			Policy::Require => "require",
		}
	}
}

impl fmt::Display for Policy {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}
