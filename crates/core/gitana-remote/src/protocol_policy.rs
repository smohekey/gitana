use std::collections::HashSet;

use anyhow::{Result, bail};
use gitana_config::GitConfig;

use crate::RemoteUrl;

/// Whether a transport was requested directly by the user or by recursive repository data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolContext {
	/// A URL supplied to a top-level clone/fetch/pull operation.
	UserInitiated,
	/// A URL discovered in repository-controlled metadata such as `.gitmodules`.
	Recursive,
}

/// The captured meaning of `GIT_PROTOCOL_FROM_USER`.
///
/// Invalid environment values are retained instead of failing command construction. They are
/// reported only if protocol authorization actually selects the `user` policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolFromUser {
	Allowed(bool),
	InvalidValue,
	InvalidEncoding,
}

/// Git-compatible protocol authorization with all process environment resolved by the caller.
#[derive(Clone, Debug)]
pub struct ProtocolPolicy {
	context: ProtocolContext,
	from_user: ProtocolFromUser,
	allowlist: Option<HashSet<String>>,
}

impl ProtocolPolicy {
	/// Construct the default policy for an invocation context.
	pub fn new(context: ProtocolContext) -> Self {
		Self {
			context,
			from_user: ProtocolFromUser::Allowed(matches!(context, ProtocolContext::UserInitiated)),
			allowlist: None,
		}
	}

	/// Apply the already-parsed meaning of `GIT_PROTOCOL_FROM_USER`.
	pub fn with_from_user(mut self, allowed: bool) -> Self {
		self.from_user = ProtocolFromUser::Allowed(allowed);
		self
	}

	/// Apply a captured `GIT_PROTOCOL_FROM_USER`, deferring invalid-value errors until needed.
	pub fn with_from_user_state(mut self, state: ProtocolFromUser) -> Self {
		self.from_user = state;
		self
	}

	/// Apply `GIT_ALLOW_PROTOCOL`'s colon-separated whitelist.
	pub fn with_allow_protocol(mut self, value: Option<&str>) -> Self {
		self.allowlist = value.map(|value| {
			value
				.split(':')
				.filter(|part| !part.is_empty())
				.map(str::to_owned)
				.collect()
		});
		self
	}

	/// Refuse `remote` unless its scheme is authorized by environment and effective config.
	pub fn authorize(&self, config: &GitConfig, remote: &RemoteUrl) -> Result<()> {
		let scheme = remote.scheme();
		if let Some(allowlist) = &self.allowlist {
			if allowlist.contains(scheme) {
				return Ok(());
			}
			bail!("transport '{scheme}' is not allowed by GIT_ALLOW_PROTOCOL");
		}

		let configured = match config.get_raw("protocol", Some(scheme), "allow") {
			Some(Some(policy)) => Some(policy),
			Some(None) => bail!("missing value for 'protocol.{scheme}.allow'"),
			None => match config.get_raw("protocol", None, "allow") {
				Some(Some(policy)) => Some(policy),
				Some(None) => bail!("missing value for 'protocol.allow'"),
				None => None,
			},
		};
		let policy = configured.unwrap_or(match scheme {
			"http" | "https" | "ssh" | "git" => "always",
			"ext" => "never",
			_ => "user",
		});
		let allowed = match policy.to_ascii_lowercase().as_str() {
			"always" => true,
			"never" => false,
			"user" => {
				let from_user = match self.from_user {
					ProtocolFromUser::Allowed(allowed) => allowed,
					ProtocolFromUser::InvalidValue => {
						bail!("bad boolean environment value for 'GIT_PROTOCOL_FROM_USER'")
					}
					ProtocolFromUser::InvalidEncoding => {
						bail!("GIT_PROTOCOL_FROM_USER is not valid UTF-8")
					}
				};
				from_user && matches!(self.context, ProtocolContext::UserInitiated)
			}
			other => bail!("unknown protocol policy '{other}' for transport '{scheme}'"),
		};
		if !allowed {
			bail!("transport '{scheme}' is not allowed in this context");
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config(input: &str) -> GitConfig {
		GitConfig::parse(input).unwrap()
	}

	#[test]
	fn file_is_user_only_by_default() {
		let remote = RemoteUrl::parse("../module").unwrap();
		assert!(
			ProtocolPolicy::new(ProtocolContext::UserInitiated)
				.authorize(&GitConfig::new(), &remote)
				.is_ok()
		);
		assert!(
			ProtocolPolicy::new(ProtocolContext::Recursive)
				.authorize(&GitConfig::new(), &remote)
				.is_err()
		);
	}

	#[test]
	fn per_scheme_config_overrides_the_default() {
		let remote = RemoteUrl::parse("../module").unwrap();
		let always = config("[protocol \"file\"]\n\tallow = always\n");
		assert!(
			ProtocolPolicy::new(ProtocolContext::Recursive)
				.authorize(&always, &remote)
				.is_ok()
		);
		let never = config("[protocol \"file\"]\n\tallow = never\n");
		assert!(
			ProtocolPolicy::new(ProtocolContext::UserInitiated)
				.authorize(&never, &remote)
				.is_err()
		);
	}

	#[test]
	fn allow_protocol_is_a_strict_whitelist() {
		let file = RemoteUrl::parse("../module").unwrap();
		let http = RemoteUrl::parse("https://example.com/repo").unwrap();
		let policy =
			ProtocolPolicy::new(ProtocolContext::Recursive).with_allow_protocol(Some("file:ssh"));
		assert!(policy.authorize(&GitConfig::new(), &file).is_ok());
		assert!(policy.authorize(&GitConfig::new(), &http).is_err());
	}

	#[test]
	fn valueless_policy_is_rejected_instead_of_falling_back() {
		let remote = RemoteUrl::parse("../module").unwrap();
		let malformed = config("[protocol \"file\"]\n\tallow\n");
		assert!(
			ProtocolPolicy::new(ProtocolContext::Recursive)
				.authorize(&malformed, &remote)
				.is_err()
		);
	}

	#[test]
	fn malformed_from_user_is_consulted_only_by_user_policy() {
		let remote = RemoteUrl::parse("../module").unwrap();
		let malformed = ProtocolPolicy::new(ProtocolContext::UserInitiated)
			.with_from_user_state(ProtocolFromUser::InvalidValue);
		assert!(
			malformed
				.authorize(&config("[protocol \"file\"]\n\tallow = always\n"), &remote)
				.is_ok()
		);
		assert!(
			malformed
				.authorize(&config("[protocol \"file\"]\n\tallow = never\n"), &remote)
				.unwrap_err()
				.to_string()
				.contains("not allowed")
		);
		assert!(
			malformed
				.authorize(&GitConfig::new(), &remote)
				.unwrap_err()
				.to_string()
				.contains("bad boolean environment value")
		);
	}
}
