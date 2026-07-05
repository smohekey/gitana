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
