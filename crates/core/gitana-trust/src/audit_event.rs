use std::fmt;

/// An audit record of a trust-policy decision.
///
/// These form the trust subsystem's audit stream (`docs/hlds/secure-git-trust-signing.md`, step 7).
/// The receive-pack enforcement path (`gitana-git-http`) produces the push-verdict variants; the
/// client `gta trust` operations (`gitana-porcelain`) produce the trust-management variants. This
/// crate owns only the *vocabulary*: v1 has no persistence — a host records the events however it
/// wishes (a log, stderr, a future signed audit ref). Each variant carries enough to render a
/// human-readable line ([`fmt::Display`]) without further lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
	/// A push passed trust verification and at least one ref moved (or there were warnings to
	/// record). `refs` are the ref names that were actually updated — a ref trust cleared but that
	/// the receive path then declined (a non-fast-forward, a denied deletion, a stale old id) is not
	/// listed here; it appears in the wire report's `ng` line instead. `warnings` is non-empty only
	/// under `warn` policy — verification failures that were recorded but not enforced.
	PushAccepted {
		/// The ref names that were updated.
		refs: Vec<String>,
		/// Failures observed but not enforced (only under `warn`).
		warnings: Vec<String>,
	},
	/// The whole push was rejected before any ref moved: a bad or missing push certificate, or a
	/// current trust root that exists but cannot be folded (a fail-closed condition). No ref changed.
	PushRejected {
		/// Why the push was rejected.
		reason: String,
	},
	/// A specific protected ref was rejected — an invalid `refs/gitana/trust` update, or an
	/// unsigned/untrusted object it newly introduced. Other refs in the same push may still have
	/// applied.
	RefRejected {
		/// The rejected ref.
		name: String,
		/// Why it was rejected.
		reason: String,
	},
}

impl fmt::Display for AuditEvent {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::PushAccepted { refs, warnings } => {
				write!(f, "push accepted ({})", refs.join(", "))?;
				for warning in warnings {
					write!(f, "; warning: {warning}")?;
				}
				Ok(())
			}
			Self::PushRejected { reason } => write!(f, "push rejected: {reason}"),
			Self::RefRejected { name, reason } => write!(f, "ref rejected {name}: {reason}"),
		}
	}
}
