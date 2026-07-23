//! [`HelperChainProvider`] is headless: it resolves a credential through git's helper chain and, when
//! the chain leaves a gap, stays anonymous rather than prompting. These drive it end-to-end through a
//! real `!`-shell credential helper (git's shell-command helper form) to prove both halves — a chain
//! that supplies a credential returns it, and a chain that supplies nothing returns `None` without ever
//! blocking on a terminal.

use gitana_config::GitConfig;
use gitana_credential::HelperChainProvider;
use gitana_remote::{CredentialProvider, CredentialRequest};

/// A Basic-challenge request for `https://example.com/repo.git`.
fn request() -> CredentialRequest {
	CredentialRequest {
		protocol: "https".to_owned(),
		host: "example.com".to_owned(),
		path: Some("repo.git".to_owned()),
		username: None,
		carried_username: None,
		wwwauth: vec!["Basic realm=\"x\"".to_owned()],
		state: Vec::new(),
		authtype: None,
		ephemeral: false,
		caps_authtype: false,
		caps_state: false,
	}
}

fn provider(config_text: &str) -> HelperChainProvider {
	let config = GitConfig::parse(config_text).expect("config parses");
	HelperChainProvider::new(config, std::env::temp_dir())
}

#[tokio::test]
async fn a_helper_that_supplies_a_credential_resolves_without_prompting() {
	// A `!`-shell helper that echoes a full Basic credential on `get` (the trailing `get` argument is
	// ignored by the wrapping function). The chain resolves it and `fill` returns it — no prompt.
	let provider = provider(
		"[credential]\n\thelper = \"!f() { echo username=alice; echo password=secret; }; f\"\n",
	);

	let filled = provider
		.fill(&request())
		.await
		.expect("fill succeeds")
		.expect("a credential is resolved");

	assert_eq!(filled.credential.username.as_deref(), Some("alice"));
	assert_eq!(filled.credential.password.as_deref(), Some("secret"));
}

#[tokio::test]
async fn a_chain_that_supplies_nothing_stays_anonymous_without_prompting() {
	// No `credential.helper` configured: the chain runs zero helpers and leaves the credential empty.
	// A headless provider must return `None` (stay anonymous, let the 401 stand) — not prompt, not block.
	// This test completing at all is the proof no tty prompt fires.
	let provider = provider("[credential]\n\tuseHttpPath = true\n");

	let filled = provider.fill(&request()).await.expect("fill succeeds");
	assert!(
		filled.is_none(),
		"a gap must resolve to anonymous, got {filled:?}"
	);
}

#[tokio::test]
async fn a_helper_that_supplies_only_a_username_leaves_a_gap_anonymous() {
	// A partial credential (username but no password) is still a gap for a headless provider: git would
	// prompt for the password, but this provider declines instead of blocking.
	let provider = provider("[credential]\n\thelper = \"!f() { echo username=alice; }; f\"\n");

	let filled = provider.fill(&request()).await.expect("fill succeeds");
	assert!(
		filled.is_none(),
		"a username-only chain must stay anonymous, got {filled:?}"
	);
}
