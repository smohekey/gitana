//! `WorktreeContext` — the injected-config carrier's own contract.
#![cfg(unix)]

use gitana_linked_worktree::{RepositoryId, WorktreeContext};

/// `Debug` must never render the injected config's contents.
///
/// A merged stack routinely carries secrets (`http.extraHeader` with an `Authorization: Bearer …`, a
/// tokenized remote URL). `GitConfig`'s derived `Debug` prints every value verbatim *and* each element's
/// `raw` source text, so a `#[derive(Debug)]` on the context would spill them into any log line or error
/// chain that includes it. This pins the hand-written impl: derive it again and this test fails.
#[test]
fn debug_redacts_the_injected_config() {
	let config = gitana_config::GitConfig::parse(
		"[http]\n\textraHeader = Authorization: Bearer SUPER_SECRET_TOKEN\n",
	)
	.unwrap();
	let repo = RepositoryId::at_common_dir(std::path::PathBuf::from("/tmp/ctx-debug-probe")).unwrap();
	let cx = WorktreeContext::with_effective_config(repo, config);

	let rendered = format!("{cx:?}");
	assert!(
		!rendered.contains("SUPER_SECRET_TOKEN"),
		"Debug leaked a credential from the injected config: {rendered}"
	);
	assert!(
		!rendered.contains("extraHeader") && !rendered.contains("extraheader"),
		"Debug leaked config keys: {rendered}"
	);
	// The useful facts survive: which repository, and that config *was* injected.
	assert!(
		rendered.contains("ctx-debug-probe"),
		"Debug still identifies the repository: {rendered}"
	);
	assert!(
		rendered.contains("redacted"),
		"Debug still reports that config was injected: {rendered}"
	);
}

/// A local-only context reports the *absence* of injected config, so the two are told apart in a log.
#[test]
fn debug_distinguishes_a_local_only_context() {
	let repo = RepositoryId::at_common_dir(std::path::PathBuf::from("/tmp/ctx-debug-local")).unwrap();
	let rendered = format!("{:?}", WorktreeContext::new(repo));
	assert!(
		rendered.contains("None"),
		"a local-only context shows no injected config: {rendered}"
	);
}
