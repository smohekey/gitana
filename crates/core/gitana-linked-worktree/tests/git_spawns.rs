//! Every git these tests run goes through `gitana_test_git::git_command()`, so nothing from the developer's
//! machine reaches it (`docs/conventions.md`, "Tests that run git").

#[test]
fn git_is_spawned_only_through_the_isolated_helper() {
	gitana_test_git::assert_git_spawned_only_through_git_command(std::path::Path::new(env!(
		"CARGO_MANIFEST_DIR"
	)));
}
