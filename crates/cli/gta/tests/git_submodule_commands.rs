//! End-to-end one-level submodule consumer operations against repositories created by Git.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

#[test]
fn update_init_materializes_the_recorded_commit_in_a_detached_worktree() {
	let fixture = Fixture::new("fresh");

	let status = gta(&fixture.consumer, false, &["submodule", "status"]);
	assert_success(&status, "initial submodule status");
	assert_eq!(stdout(&status), format!("-{} modules/one\n", fixture.old));

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "submodule update --init");
	assert_eq!(
		stdout(&update),
		format!(
			"Submodule path 'modules/one': checked out '{}'\n",
			fixture.old
		)
	);
	assert!(
		stderr(&update).contains("Submodule 'one'"),
		"registration is reported: {}",
		stderr(&update)
	);
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "--abbrev-ref", "HEAD"]
		)
		.trim(),
		"HEAD",
		"the module is checked out with detached HEAD"
	);
	assert_eq!(
		stdout(&gta(&fixture.consumer, false, &["submodule", "status"])),
		format!(" {} modules/one\n", fixture.old)
	);
	assert_mount_points_at_per_worktree_repository(&fixture.consumer);
}

#[test]
fn nested_module_names_publish_the_complete_repository_and_mount_namespace() {
	let fixture = Fixture::new("nested-module-name");
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"--rename-section",
			"submodule.one",
			"submodule.modules/one",
		],
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "nested-name submodule update");
	assert!(git_path(&fixture.consumer, "modules/modules/one").is_dir());
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn recursive_protocol_denial_does_not_publish_a_module_repository() {
	let fixture = Fixture::new("deny");
	let denied = gta_with_config(
		&fixture.consumer,
		"protocol.file.allow=never",
		&["submodule", "update", "--init"],
	);
	assert!(!denied.status.success(), "protocol denial must fail");
	assert!(
		stderr(&denied).contains("transport 'file' is not allowed"),
		"unexpected error: {}",
		stderr(&denied)
	);

	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	assert!(
		!module_git_dir.exists(),
		"a denied transfer must not publish the module repository"
	);
	assert!(
		!fixture.consumer.join("modules/one/.git").exists(),
		"a denied transfer must not mount the staged repository"
	);
	assert_eq!(
		git(
			&fixture.consumer,
			&["config", "--get", "submodule.one.active"]
		)
		.trim(),
		"true",
		"Git-compatible --init registration precedes transport"
	);
}

#[test]
fn source_validation_failure_publishes_no_recovery_state_and_allows_correction() {
	let fixture = Fixture::new("intent-only-source-correction");
	let invalid = "bogus://example.invalid/module";
	git_ok(
		&fixture.consumer,
		&["config", "-f", ".gitmodules", "submodule.one.url", invalid],
	);

	let failed = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(
		!failed.status.success(),
		"invalid source must fail preparation"
	);
	let control = git_path(&fixture.consumer, "gitana-submodule-update");
	assert!(
		!control.exists(),
		"source validation must finish before recovery state is published"
	);
	assert!(
		!git_path(&fixture.consumer, "modules/one").exists(),
		"source validation must not publish a repository"
	);

	git_ok(
		&fixture.consumer,
		&[
			"config",
			"submodule.one.url",
			fixture.source.to_str().unwrap(),
		],
	);
	let retry = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&retry, "retry after correcting an intent-only source");
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
	assert!(
		!control.exists(),
		"the fresh successful attempt must clear recovery control state"
	);
}

#[test]
fn source_correction_cannot_rebind_an_existing_recovery_repository() {
	let fixture = Fixture::new("repository-source-binding");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let original = git(&fixture.consumer, &["config", "--get", "submodule.one.url"])
		.trim()
		.to_owned();
	let control = write_v2_recovery_intent(&fixture, "one", "modules/one", &fixture.old, &original);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"submodule.one.url",
			fixture.superproject.to_str().unwrap(),
		],
	);

	let retry = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert!(
		!retry.status.success(),
		"published recovery source mismatch must fail"
	);
	assert!(
		stderr(&retry).contains("unfinished staging source does not match 'one'"),
		"unexpected error: {}",
		stderr(&retry)
	);
	assert!(control.join("intent.json").is_file());
	assert!(git_path(&fixture.consumer, "modules/one").is_dir());
}

#[test]
fn rewrite_change_cannot_rebind_an_existing_recovery_repository() {
	let fixture = Fixture::new("repository-rewrite-binding");
	let alias = "module-alias:";
	git_ok(
		&fixture.consumer,
		&["config", "-f", ".gitmodules", "submodule.one.url", alias],
	);
	let original_key = format!("url.{}.insteadOf", fixture.source.display());
	git_ok(&fixture.consumer, &["config", &original_key, alias]);
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial rewritten update",
	);
	let control = write_v3_recovery_intent(
		&fixture,
		"one",
		"modules/one",
		&fixture.old,
		fixture.source.to_str().unwrap(),
	);

	git_ok(&fixture.consumer, &["config", "--unset-all", &original_key]);
	let replacement_key = format!("url.{}.insteadOf", fixture.superproject.display());
	git_ok(&fixture.consumer, &["config", &replacement_key, alias]);

	let retry = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert!(
		!retry.status.success(),
		"a changed rewrite endpoint must not complete old recovery state"
	);
	assert!(
		stderr(&retry).contains("unfinished staging source does not match 'one'"),
		"unexpected error: {}",
		stderr(&retry)
	);
	assert!(control.join("intent.json").is_file());
	assert!(git_path(&fixture.consumer, "modules/one").is_dir());
}

#[test]
fn initial_transfer_uses_repository_local_protocol_and_url_rewrite_config() {
	let fixture = Fixture::new("local-initial-transfer-config");
	let alias = "file://invalid-authority/module";
	git_ok(
		&fixture.consumer,
		&["config", "-f", ".gitmodules", "submodule.one.url", alias],
	);
	git_ok(
		&fixture.consumer,
		&["config", "protocol.file.allow", "always"],
	);
	let rewrite_key = format!("url.{}.insteadOf", fixture.source.display());
	git_ok(&fixture.consumer, &["config", &rewrite_key, alias]);

	let update = gta(&fixture.consumer, false, &["submodule", "update", "--init"]);
	assert_success(&update, "initial transfer with repository-local config");
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
}

#[test]
fn initial_transfer_honors_repository_local_protocol_denial() {
	let fixture = Fixture::new("local-initial-transfer-denial");
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			"http://127.0.0.1:1/module",
		],
	);
	git_ok(
		&fixture.consumer,
		&["config", "protocol.http.allow", "never"],
	);

	let update = gta(&fixture.consumer, false, &["submodule", "update", "--init"]);
	assert!(!update.status.success(), "local protocol denial must fail");
	assert!(
		stderr(&update).contains("transport 'http' is not allowed"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert!(!git_path(&fixture.consumer, "modules/one").exists());
	assert!(!fixture.consumer.join("modules/one/.git").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewritten_http_module_origin_is_credential_safe_and_reusable() {
	if !support::git_http_backend_available() {
		eprintln!("skipping: git http-backend not available");
		return;
	}
	let fixture = Fixture::new("http-initial-transfer-rewrite");
	let base = support::serve_git_http_backend(fixture.root.clone()).await;
	let rewritten = format!(
		"http://alice:secret@{}/source",
		base.trim_start_matches("http://")
	);
	let persisted = format!("http://alice@{}/source", base.trim_start_matches("http://"));
	let alias = "module-alias:";
	git_ok(
		&fixture.consumer,
		&["config", "-f", ".gitmodules", "submodule.one.url", alias],
	);
	let rewrite_key = format!("url.{rewritten}.insteadOf");
	git_ok(&fixture.consumer, &["config", &rewrite_key, alias]);

	let initial = gta(&fixture.consumer, false, &["submodule", "update", "--init"]);
	assert_success(&initial, "initial rewritten HTTP submodule update");
	let module = fixture.consumer.join("modules/one");
	assert_eq!(
		git(&module, &["config", "--get", "remote.origin.url"]).trim(),
		persisted,
		"the module must retain the effective endpoint without its password"
	);
	git_ok(&fixture.consumer, &["config", "--unset-all", &rewrite_key]);

	let next = fixture.commit_source("rewritten HTTP next\n", "rewritten HTTP next");
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{next},modules/one"),
		],
	);
	let update = gta(&fixture.consumer, false, &["submodule", "update"]);
	assert_success(
		&update,
		"later module update without the superproject rewrite",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), next);
}

#[test]
fn command_scope_url_and_strategy_override_local_submodule_config() {
	let fixture = Fixture::new("command-config-precedence");
	git_ok(
		&fixture.consumer,
		&["config", "submodule.one.url", "../missing-local-source"],
	);
	git_ok(
		&fixture.consumer,
		&["config", "submodule.one.update", "none"],
	);
	let source = format!("submodule.one.url={}", fixture.source.display());
	let update = gta_with_configs(
		&fixture.consumer,
		&[
			"protocol.file.allow=always",
			&source,
			"submodule.one.update=checkout",
		],
		&["submodule", "update", "--init"],
	);

	assert_success(&update, "command-scope submodule overrides");
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
}

#[test]
fn command_scope_activation_overrides_persistent_and_initialized_values() {
	let disabled = Fixture::new("command-active-false");
	let update = gta_with_configs(
		&disabled.consumer,
		&["protocol.file.allow=always", "submodule.one.active=false"],
		&["submodule", "update", "--init"],
	);
	assert_success(&update, "command-scope inactive update");
	assert_eq!(
		git(
			&disabled.consumer,
			&["config", "--get", "submodule.one.active"]
		)
		.trim(),
		"true",
		"init still records persistent activation"
	);
	assert!(
		!git_path(&disabled.consumer, "modules/one").exists(),
		"the effective command override must skip repository creation"
	);

	let enabled = Fixture::new("command-active-true");
	git_ok(
		&enabled.consumer,
		&["config", "submodule.one.active", "false"],
	);
	let update = gta_with_configs(
		&enabled.consumer,
		&["protocol.file.allow=always", "submodule.one.active=true"],
		&["submodule", "update", "--init"],
	);
	assert_success(&update, "command-scope active update");
	assert_eq!(
		git(
			&enabled.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		enabled.old
	);
}

#[test]
fn malformed_protocol_from_user_is_deferred_until_a_user_policy_is_authorized() {
	let fixture = Fixture::new("deferred-protocol-from-user");
	let malformed = [("GIT_PROTOCOL_FROM_USER", "not-a-boolean")];

	let status = gta_with_environment(&fixture.consumer, &["submodule", "status"], &malformed);
	assert_success(&status, "status with malformed protocol environment");
	let init = gta_with_environment(&fixture.consumer, &["submodule", "init"], &malformed);
	assert_success(&init, "init with malformed protocol environment");

	let always_environment = [
		("GIT_PROTOCOL_FROM_USER", "not-a-boolean"),
		("GIT_CONFIG_COUNT", "1"),
		("GIT_CONFIG_KEY_0", "protocol.file.allow"),
		("GIT_CONFIG_VALUE_0", "always"),
	];
	let always = gta_with_environment(
		&fixture.consumer,
		&["submodule", "update"],
		&always_environment,
	);
	assert_success(&always, "always-policy transfer");

	let user_environment = [
		("GIT_PROTOCOL_FROM_USER", "not-a-boolean"),
		("GIT_CONFIG_COUNT", "1"),
		("GIT_CONFIG_KEY_0", "protocol.file.allow"),
		("GIT_CONFIG_VALUE_0", "user"),
	];
	let user = gta_with_environment(
		&fixture.consumer,
		&["submodule", "update"],
		&user_environment,
	);
	assert!(
		!user.status.success(),
		"user policy must evaluate the malformed value"
	);
	assert!(
		stderr(&user).contains("bad boolean environment value"),
		"unexpected user-policy error: {}",
		stderr(&user)
	);
}

#[test]
fn top_level_local_clone_and_pull_use_the_in_process_transport() {
	let fixture = Fixture::new("top-level-local");
	let clone = fixture.root.join("local-clone");
	let cloned = gta(
		&fixture.root,
		false,
		&["clone", "source", clone.to_str().unwrap()],
	);
	assert_success(&cloned, "top-level local clone");
	let canonical_source = std::fs::canonicalize(&fixture.source).unwrap();
	assert_eq!(
		git(&clone, &["config", "--get", "remote.origin.url"]).trim(),
		canonical_source.to_str().unwrap()
	);
	assert!(
		git(&clone, &["reflog", "-1", "--format=%gs"]).contains(canonical_source.to_str().unwrap()),
		"a direct relative clone reflog must use the resolved source"
	);
	assert_eq!(
		std::fs::read_to_string(clone.join("file.txt")).unwrap(),
		"old\n"
	);

	let next = fixture.commit_source("next\n", "next");
	let pulled = gta(&clone, false, &["pull"]);
	assert_success(&pulled, "top-level local pull");
	assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), next);
	assert_eq!(
		std::fs::read_to_string(clone.join("file.txt")).unwrap(),
		"next\n"
	);
}

#[test]
fn omitted_local_clone_destination_preserves_dot_path_semantics() {
	let fixture = Fixture::new("omitted-local-clone-destination");
	let dot_clone = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&fixture.source)
		.args(["clone", "."])
		.output()
		.expect("clone current repository");
	assert!(
		!dot_clone.status.success(),
		"clone . must target the existing current directory"
	);
	assert!(stderr(&dot_clone).contains("already exists and is not empty"));
	assert!(!fixture.source.join("source").exists());

	let nested = fixture.source.join("nested");
	std::fs::create_dir(&nested).unwrap();
	let parent_clone = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&nested)
		.args(["clone", "../"])
		.output()
		.expect("clone parent repository");
	assert!(
		!parent_clone.status.success(),
		"clone .. must target the existing parent directory"
	);
	assert!(stderr(&parent_clone).contains("already exists and is not empty"));
	assert!(!nested.join("source").exists());

	let dotted_source = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&fixture.root)
		.args(["clone", "source/."])
		.output()
		.expect("clone repository through a trailing dot");
	assert!(
		!dotted_source.status.success(),
		"clone source/. must target the existing current directory"
	);
	assert!(stderr(&dotted_source).contains("already exists and is not empty"));

	let git_dir_parent = fixture.root.join("git-dir-clone");
	std::fs::create_dir(&git_dir_parent).unwrap();
	let source_git_dir = fixture.source.join(".git");
	let git_dir_clone = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&git_dir_parent)
		.arg("clone")
		.arg(&source_git_dir)
		.output()
		.expect("clone repository git directory");
	assert_success(
		&git_dir_clone,
		"clone a repository git directory without a destination",
	);
	let git_dir_target = git_dir_parent.join("source");
	assert_eq!(
		std::fs::read_to_string(git_dir_target.join("file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn local_clone_destinations_are_resolved_against_the_effective_command_directory() {
	let fixture = Fixture::new("local-clone-command-directory");
	let launch = fixture.root.join("launch");
	let explicit_base = fixture.root.join("explicit-base");
	let inferred_base = fixture.root.join("inferred-base");
	std::fs::create_dir(&launch).unwrap();
	std::fs::create_dir(&explicit_base).unwrap();
	std::fs::create_dir(&inferred_base).unwrap();

	let explicit = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&launch)
		.args(["-C", explicit_base.to_str().unwrap()])
		.args(["clone", "../source", "out"])
		.output()
		.expect("clone with an explicit relative destination under -C");
	assert_success(&explicit, "explicit clone destination under -C");
	assert_eq!(
		std::fs::read_to_string(explicit_base.join("out/file.txt")).unwrap(),
		"old\n"
	);
	assert!(!launch.join("out").exists());

	let inferred = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&launch)
		.args(["-C", inferred_base.to_str().unwrap()])
		.args(["clone", "../source"])
		.output()
		.expect("clone with an inferred destination under -C");
	assert_success(&inferred, "inferred clone destination under -C");
	assert_eq!(
		std::fs::read_to_string(inferred_base.join("source/file.txt")).unwrap(),
		"old\n"
	);
	assert!(!launch.join("source").exists());

	let nested = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&launch)
		.args(["-C", explicit_base.to_str().unwrap()])
		.args(["clone", "../source", "nested/out"])
		.output()
		.expect("clone with a nested destination under an existing -C directory");
	assert_success(&nested, "nested clone destination under -C");
	assert_eq!(
		std::fs::read_to_string(explicit_base.join("nested/out/file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn local_clone_rejects_a_missing_or_non_directory_command_directory() {
	let fixture = Fixture::new("local-clone-invalid-command-directory");
	let missing = fixture.root.join("missing-command-directory");
	let missing_output = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&fixture.root)
		.args(["-C", missing.to_str().unwrap()])
		.args(["clone", fixture.source.to_str().unwrap(), "out"])
		.output()
		.expect("clone with a missing -C directory");
	assert!(!missing_output.status.success());
	assert!(
		!missing.exists(),
		"a missing -C directory must not be created by clone"
	);

	let file = fixture.root.join("command-directory-file");
	std::fs::write(&file, "not a directory\n").unwrap();
	let file_output = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.current_dir(&fixture.root)
		.args(["-C", file.to_str().unwrap()])
		.args(["clone", fixture.source.to_str().unwrap(), "out"])
		.output()
		.expect("clone with a file-valued -C directory");
	assert!(!file_output.status.success());
	assert_eq!(std::fs::read_to_string(&file).unwrap(), "not a directory\n");
}

#[test]
fn shallow_local_sources_propagate_their_boundary_to_clone_fetch_and_submodules() {
	let fixture = Fixture::new("shallow-local-source");
	fixture.commit_source("middle\n", "middle");
	fixture.commit_source("tip\n", "tip");
	let shallow_source = fixture.root.join("shallow-source");
	let source_url = format!("file://{}", fixture.source.display());
	let cloned = Command::new("git")
		.args(["clone", "-q", "--depth=2", &source_url])
		.arg(&shallow_source)
		.output()
		.expect("create a stock-Git shallow source");
	assert_success(&cloned, "create shallow source");
	let source_boundary = git(&shallow_source, &["rev-parse", "HEAD^"])
		.trim()
		.to_owned();

	let clone = fixture.root.join("shallow-clone");
	let cloned = gta(
		&fixture.root,
		true,
		&[
			"clone",
			shallow_source.to_str().unwrap(),
			clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "clone from a shallow local source");
	assert_eq!(
		git(&clone, &["rev-parse", "--is-shallow-repository"]).trim(),
		"true"
	);
	assert_eq!(git(&clone, &["rev-list", "--count", "HEAD"]).trim(), "2");
	assert_eq!(
		std::fs::read_to_string(clone.join(".git/shallow"))
			.unwrap()
			.trim(),
		source_boundary
	);

	std::fs::write(shallow_source.join("file.txt"), b"shallow next\n").unwrap();
	git_ok(&shallow_source, &["add", "file.txt"]);
	commit(&shallow_source, "shallow next");
	let next = git(&shallow_source, &["rev-parse", "HEAD"])
		.trim()
		.to_owned();
	let fetched = gta(&clone, true, &["fetch"]);
	assert_success(&fetched, "fetch from a shallow local source");
	assert_eq!(
		git(&clone, &["rev-parse", "refs/remotes/origin/main"]).trim(),
		next
	);
	assert_eq!(
		std::fs::read_to_string(clone.join(".git/shallow"))
			.unwrap()
			.trim(),
		source_boundary
	);

	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			shallow_source.to_str().unwrap(),
		],
	);
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{next},modules/one"),
		],
	);
	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(
		&update,
		"materialize a submodule from a shallow local source",
	);
	let module = fixture.consumer.join("modules/one");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), next);
	assert_eq!(
		git(&module, &["rev-parse", "--is-shallow-repository"]).trim(),
		"true"
	);
	assert_eq!(
		std::fs::read_to_string(git_path(&fixture.consumer, "modules/one").join("shallow"))
			.unwrap()
			.trim(),
		source_boundary
	);
}

#[test]
fn shallow_source_exact_fallback_cannot_cross_its_boundary() {
	let fixture = Fixture::new("shallow-exact-boundary");
	let boundary = fixture.commit_source("boundary\n", "boundary");
	fixture.commit_source("tip\n", "tip");
	let shallow_file = git_path(&fixture.source, "shallow");
	std::fs::write(&shallow_file, format!("{boundary}\n")).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(
		!update.status.success(),
		"a retained commit below the source boundary must not be fetched: {}",
		stderr(&update)
	);
	assert!(
		stderr(&update).contains("not reachable within the source repository's shallow boundary"),
		"the source-boundary rejection must be reported: {}",
		stderr(&update)
	);
	assert!(
		!git_path(&fixture.consumer, "modules/one").exists(),
		"the rejected staged repository must not be published"
	);
	assert!(
		!fixture.consumer.join("modules/one/.git").exists(),
		"the rejected module must not publish a mount marker"
	);

	std::fs::remove_file(shallow_file).unwrap();
	let retry = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&retry, "retry after the source becomes complete");
	let module = fixture.consumer.join("modules/one");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn fetching_from_a_newly_shallow_source_keeps_complete_destinations_complete() {
	let fixture = Fixture::new("complete-destination-shallow-source");
	let boundary = fixture.commit_source("boundary\n", "boundary");
	let gta_clone = fixture.root.join("gta-complete-clone");
	let git_clone = fixture.root.join("git-complete-clone");

	let cloned = gta(
		&fixture.root,
		true,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			gta_clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "create complete GTA destination");
	let cloned = Command::new("git")
		.args(["clone", "-q"])
		.arg(&fixture.source)
		.arg(&git_clone)
		.output()
		.expect("create complete Git destination");
	assert_success(&cloned, "create complete Git destination");

	let tip = fixture.commit_source("tip\n", "tip");
	std::fs::write(
		git_path(&fixture.source, "shallow"),
		format!("{boundary}\n"),
	)
	.unwrap();

	let fetched = gta(&gta_clone, true, &["fetch"]);
	assert_success(&fetched, "fetch into complete GTA destination");
	git_allow(&git_clone, &["fetch"]);

	for destination in [&gta_clone, &git_clone] {
		assert_eq!(
			git(destination, &["rev-parse", "refs/remotes/origin/main"]).trim(),
			tip
		);
		assert_eq!(
			git(destination, &["rev-parse", "--is-shallow-repository"]).trim(),
			"false"
		);
		assert!(
			!git_path(destination, "shallow").exists(),
			"{} unexpectedly acquired a shallow marker",
			destination.display()
		);
		assert_eq!(
			git(
				destination,
				&["rev-list", "--count", "refs/remotes/origin/main"]
			)
			.trim(),
			"3"
		);
	}
}

#[test]
fn mixed_case_localhost_file_urls_clone_and_pull_through_the_local_transport() {
	let fixture = Fixture::new("mixed-case-localhost-file-url");
	let clone = fixture.root.join("localhost-clone");
	let url = format!("file://LOCALHOST{}", fixture.source.display());

	let cloned = gta(
		&fixture.root,
		true,
		&["clone", &url, clone.to_str().unwrap()],
	);
	assert_success(&cloned, "clone through mixed-case localhost authority");
	assert_eq!(
		git(&clone, &["config", "--get", "remote.origin.url"]).trim(),
		url
	);

	let next = fixture.commit_source("localhost next\n", "localhost next");
	let pulled = gta(&clone, true, &["pull"]);
	assert_success(&pulled, "pull through mixed-case localhost authority");
	assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), next);
}

#[cfg(unix)]
#[test]
fn top_level_local_clone_preserves_a_preexisting_destination_identity_and_metadata() {
	use std::os::unix::fs::{MetadataExt, PermissionsExt};

	let fixture = Fixture::new("top-level-local-existing-destination-metadata");
	let clone = fixture.root.join("private-clone");
	std::fs::create_dir(&clone).unwrap();
	std::fs::set_permissions(&clone, std::fs::Permissions::from_mode(0o700)).unwrap();
	let before = std::fs::metadata(&clone).unwrap();

	let cloned = gta(
		&fixture.root,
		false,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "clone into a pre-existing private directory");

	let after = std::fs::metadata(&clone).unwrap();
	assert_eq!(after.dev(), before.dev());
	assert_eq!(after.ino(), before.ino());
	assert_eq!(after.uid(), before.uid());
	assert_eq!(after.gid(), before.gid());
	assert_eq!(after.permissions().mode() & 0o777, 0o700);
	assert_eq!(
		std::fs::read_to_string(clone.join("file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn top_level_local_clone_preserves_a_rewrite_alias() {
	let fixture = Fixture::new("top-level-local-alias");
	let clone = fixture.root.join("alias-clone");
	let rewrite = format!(
		"url.file://{}/.insteadOf=alias:",
		fixture.root.to_str().unwrap()
	);
	let cloned = gta_with_config(
		&fixture.root,
		&rewrite,
		&["clone", "alias:source", clone.to_str().unwrap()],
	);
	assert_success(&cloned, "top-level rewritten local clone");
	assert_eq!(
		git(&clone, &["config", "--get", "remote.origin.url"]).trim(),
		"alias:source"
	);

	let next = fixture.commit_source("alias next\n", "alias next");
	let pulled = gta_with_config(&clone, &rewrite, &["pull"]);
	assert_success(&pulled, "pull through retained local alias");
	assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), next);
}

#[test]
fn top_level_local_clone_preserves_an_unreferenced_detached_head() {
	let fixture = Fixture::new("top-level-detached-head");
	let detached = fixture.commit_source("detached\n", "detached source head");
	git_ok(&fixture.source, &["checkout", "-q", "--detach", &detached]);
	git_ok(&fixture.source, &["branch", "-f", "main", &fixture.old]);

	let clone = fixture.root.join("detached-clone");
	let cloned = gta(
		&fixture.root,
		false,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "clone a detached local source");
	assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), detached);
	assert_eq!(
		std::fs::read_to_string(clone.join("file.txt")).unwrap(),
		"detached\n"
	);
	let symbolic = Command::new("git")
		.args(["-C", clone.to_str().unwrap(), "symbolic-ref", "-q", "HEAD"])
		.output()
		.expect("inspect cloned HEAD");
	assert!(
		!symbolic.status.success(),
		"the destination HEAD must be detached"
	);
	let reflog = std::fs::read_to_string(clone.join(".git/logs/HEAD")).unwrap();
	let entries: Vec<&str> = reflog.lines().collect();
	assert_eq!(entries.len(), 1, "detached clone writes one HEAD entry");
	let fields: Vec<&str> = entries[0].split_whitespace().collect();
	assert_eq!(fields[0], "0".repeat(detached.len()));
	assert_eq!(fields[1], detached);
}

#[test]
fn failed_local_clone_cleans_its_destination_and_preserves_an_existing_empty_directory() {
	let fixture = Fixture::new("top-level-local-cleanup");
	let broken = "f".repeat(fixture.old.len());
	let broken_ref = fixture.source.join(".git/refs/heads/broken");
	std::fs::write(&broken_ref, format!("{broken}\n")).unwrap();

	let created = fixture.root.join("failed-created-clone");
	let failed = gta(
		&fixture.root,
		false,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			created.to_str().unwrap(),
		],
	);
	assert!(
		!failed.status.success(),
		"the invalid advertised ref must fail clone"
	);
	assert!(
		!created.exists(),
		"an attempt-created destination must be removed"
	);

	let existing = fixture.root.join("failed-existing-clone");
	std::fs::create_dir(&existing).unwrap();
	let failed = gta(
		&fixture.root,
		false,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			existing.to_str().unwrap(),
		],
	);
	assert!(
		!failed.status.success(),
		"the invalid advertised ref must fail clone"
	);
	assert!(
		existing.is_dir(),
		"the originally empty destination must remain"
	);
	assert_eq!(
		std::fs::read_dir(&existing).unwrap().count(),
		0,
		"the retained destination must be empty after cleanup"
	);

	std::fs::remove_file(broken_ref).unwrap();
	let retry = gta(
		&fixture.root,
		false,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			existing.to_str().unwrap(),
		],
	);
	assert_success(&retry, "retry clone into the cleaned destination");
}

#[cfg(unix)]
#[test]
fn local_clone_rejects_and_preserves_a_dangling_destination_symlink() {
	use std::os::unix::fs::symlink;

	let fixture = Fixture::new("top-level-local-dangling-destination");
	let target = fixture.root.join("dangling-clone");
	let link_target = fixture.root.join("missing-target");
	symlink(&link_target, &target).unwrap();

	let cloned = gta(
		&fixture.root,
		false,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			target.to_str().unwrap(),
		],
	);
	assert!(!cloned.status.success(), "a symlink destination must fail");
	assert!(
		std::fs::symlink_metadata(&target)
			.unwrap()
			.file_type()
			.is_symlink(),
		"the pre-existing symlink must survive failure"
	);
	assert_eq!(std::fs::read_link(&target).unwrap(), link_target);
}

#[test]
fn shallow_options_are_ignored_for_local_paths_but_honored_for_file_urls() {
	let fixture = Fixture::new("top-level-local-shallow");
	fixture.commit_source("new\n", "second source commit");

	let path_clone = fixture.root.join("path-depth-clone");
	let cloned = gta(
		&fixture.root,
		false,
		&[
			"clone",
			"--depth",
			"1",
			"source",
			path_clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "depth-limited local-path clone");
	assert_eq!(
		git(&path_clone, &["rev-list", "--count", "HEAD"]).trim(),
		"2"
	);
	assert_eq!(
		git(&path_clone, &["rev-parse", "--is-shallow-repository"]).trim(),
		"false"
	);

	let file_clone = fixture.root.join("file-url-depth-clone");
	let file_url = format!("file://{}", fixture.source.display());
	let cloned = gta(
		&fixture.root,
		false,
		&[
			"clone",
			"--depth",
			"1",
			&file_url,
			file_clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "depth-limited file URL clone");
	assert_eq!(
		git(&file_clone, &["rev-list", "--count", "HEAD"]).trim(),
		"1"
	);
	assert_eq!(
		git(&file_clone, &["rev-parse", "--is-shallow-repository"]).trim(),
		"true"
	);
}

#[test]
fn local_transports_require_the_named_path_to_be_a_repository_root() {
	let fixture = Fixture::new("local-exact-root");
	let nested_source = fixture.source.join("ordinary-directory");
	std::fs::create_dir(&nested_source).unwrap();
	let rejected_clone = fixture.root.join("rejected-clone");

	let clone = gta(
		&fixture.root,
		false,
		&[
			"clone",
			nested_source.to_str().unwrap(),
			rejected_clone.to_str().unwrap(),
		],
	);
	assert!(
		!clone.status.success(),
		"a repository subdirectory must not clone"
	);
	assert!(
		stderr(&clone).contains("is not a repository root"),
		"unexpected clone error: {}",
		stderr(&clone)
	);
	assert!(
		!rejected_clone.exists(),
		"exact-root validation must precede destination creation"
	);

	let client = fixture.root.join("local-client");
	assert_success(
		&gta(
			&fixture.root,
			false,
			&[
				"clone",
				fixture.source.to_str().unwrap(),
				client.to_str().unwrap(),
			],
		),
		"valid local clone",
	);
	git_ok(
		&client,
		&[
			"config",
			"remote.origin.url",
			nested_source.to_str().unwrap(),
		],
	);
	let head_before = git(&client, &["rev-parse", "HEAD"]);
	let refs_before = git(
		&client,
		&["for-each-ref", "--format=%(refname) %(objectname)"],
	);
	for command in ["fetch", "pull"] {
		let output = gta(&client, false, &[command]);
		assert!(
			!output.status.success(),
			"local {command} must reject a subdirectory"
		);
		assert!(
			stderr(&output).contains("is not a repository root"),
			"unexpected {command} error: {}",
			stderr(&output)
		);
		assert_eq!(git(&client, &["rev-parse", "HEAD"]), head_before);
		assert_eq!(
			git(
				&client,
				&["for-each-ref", "--format=%(refname) %(objectname)"]
			),
			refs_before
		);
	}

	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			nested_source.to_str().unwrap(),
		],
	);
	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(
		!update.status.success(),
		"submodule transfer must reject a repository subdirectory"
	);
	assert!(
		stderr(&update).contains("is not a repository root"),
		"unexpected submodule error: {}",
		stderr(&update)
	);
	assert!(!git_path(&fixture.consumer, "modules/one").exists());
	assert!(!fixture.consumer.join("modules/one/.git").exists());
}

#[test]
fn top_level_local_clone_accepts_an_exact_bare_repository_root() {
	let fixture = Fixture::new("local-bare-root");
	let bare = fixture.root.join("source.git");
	let bare_clone = Command::new("git")
		.args(["clone", "-q", "--bare"])
		.arg(&fixture.source)
		.arg(&bare)
		.output()
		.expect("clone bare source");
	assert!(
		bare_clone.status.success(),
		"clone bare source: {}",
		stderr(&bare_clone)
	);
	let checkout = fixture.root.join("bare-client");
	let clone = gta(
		&fixture.root,
		false,
		&["clone", bare.to_str().unwrap(), checkout.to_str().unwrap()],
	);
	assert_success(&clone, "clone exact bare repository root");
	assert_eq!(
		std::fs::read_to_string(checkout.join("file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn an_existing_dirty_module_is_not_moved_until_a_clean_retry() {
	let fixture = Fixture::new("dirty");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let next = fixture.commit_source("upstream\n", "next");
	fixture.record_superproject_commit(&next, "advance module");
	git_ok(&fixture.consumer, &["pull", "--ff-only"]);

	let module = fixture.consumer.join("modules/one");
	std::fs::write(module.join("file.txt"), b"local change\n").unwrap();
	let failed = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert!(!failed.status.success(), "dirty checkout must fail");
	assert!(
		stderr(&failed).contains("overwrite local changes"),
		"unexpected dirty-checkout error: {}",
		stderr(&failed)
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"local change\n"
	);

	git_ok(&module, &["checkout", "--", "file.txt"]);
	let retry = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&retry, "clean update retry");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), next);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"upstream\n"
	);
	assert_eq!(
		git(&module, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"HEAD"
	);
}

#[test]
fn an_existing_unborn_module_is_refused_without_changing_local_state() {
	let fixture = Fixture::new("unborn-existing");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	std::fs::write(module.join("file.txt"), b"local unborn change\n").unwrap();
	git_ok(&module, &["symbolic-ref", "HEAD", "refs/heads/unborn"]);

	let head_before = std::fs::read(module_git_dir.join("HEAD")).unwrap();
	let index_before = std::fs::read(module_git_dir.join("index")).unwrap();
	let config_before = std::fs::read(module_git_dir.join("config")).unwrap();
	let marker_before = std::fs::read(module.join(".git")).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert!(!update.status.success(), "unborn existing module must fail");
	assert!(
		stderr(&update).contains("because its HEAD is unborn"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"local unborn change\n"
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("HEAD")).unwrap(),
		head_before
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("index")).unwrap(),
		index_before
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("config")).unwrap(),
		config_before
	);
	assert_eq!(std::fs::read(module.join(".git")).unwrap(), marker_before);
}

#[test]
fn module_checkout_honors_command_scope_reflog_policy() {
	let fixture = Fixture::new("module-effective-reflog-config");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let head_log = module_git_dir.join("logs/HEAD");
	assert!(
		head_log.exists(),
		"the initial checkout should create a HEAD reflog"
	);

	let next = fixture.commit_source("next\n", "next source commit");
	fixture.record_superproject_commit(&next, "advance module");
	git_ok(&fixture.consumer, &["pull", "--ff-only"]);
	std::fs::remove_file(&head_log).unwrap();

	let update = gta_with_configs(
		&fixture.consumer,
		&["protocol.file.allow=always", "core.logAllRefUpdates=false"],
		&["submodule", "update"],
	);
	assert_success(&update, "update with command-scope reflog policy");
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		next
	);
	assert!(
		!head_log.exists(),
		"command-scope core.logAllRefUpdates=false must suppress HEAD reflog recreation"
	);
}

#[test]
fn new_module_does_not_inherit_superproject_local_operational_config() {
	let fixture = Fixture::new("module-config-boundary");
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"core.logAllRefUpdates",
			"definitely-not-a-boolean",
		],
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(
		&update,
		"new module must not inherit superproject-local reflog policy",
	);
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
}

#[test]
fn module_head_publication_is_preflighted_before_checkout_mutation() {
	let fixture = Fixture::new("module-head-preflight");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let old_index = std::fs::read(module_git_dir.join("index")).unwrap();

	let next = fixture.commit_source("next\n", "next source commit");
	fixture.record_superproject_commit(&next, "advance module");
	git_ok(&fixture.consumer, &["pull", "--ff-only"]);
	let head_log = module_git_dir.join("logs/HEAD");
	std::fs::remove_file(&head_log).unwrap();
	std::fs::create_dir(&head_log).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert!(
		!update.status.success(),
		"the blocked HEAD reflog must fail"
	);
	assert!(
		stderr(&update).contains("reflog path blocked"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"old\n"
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("index")).unwrap(),
		old_index,
		"HEAD preflight failure must leave the module index untouched"
	);
}

#[test]
fn module_merge_checkout_honors_configured_and_default_global_excludes() {
	for (tag, use_xdg_default) in [
		("module-configured-excludes", false),
		("module-xdg-excludes", true),
	] {
		let fixture = Fixture::new(tag);
		assert_success(
			&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
			"initial update",
		);
		std::fs::write(fixture.source.join("ignored.txt"), b"upstream\n").unwrap();
		git_ok(&fixture.source, &["add", "ignored.txt"]);
		commit(&fixture.source, "add ignored path");
		let next = git(&fixture.source, &["rev-parse", "HEAD"])
			.trim()
			.to_owned();
		fixture.record_superproject_commit(&next, "advance module to ignored path");
		git_ok(&fixture.consumer, &["pull", "--ff-only"]);

		let module = fixture.consumer.join("modules/one");
		std::fs::write(module.join("ignored.txt"), b"ignored local obstruction\n").unwrap();
		let excludes = fixture.root.join("global-ignore");
		std::fs::write(&excludes, b"ignored.txt\n").unwrap();
		let update = if use_xdg_default {
			let xdg = fixture.root.join("xdg");
			std::fs::create_dir_all(xdg.join("git")).unwrap();
			std::fs::copy(&excludes, xdg.join("git/ignore")).unwrap();
			let xdg = xdg.to_str().unwrap();
			gta_with_environment(
				&fixture.consumer,
				&["submodule", "update"],
				&[
					("GIT_ALLOW_PROTOCOL", "file"),
					("GIT_CONFIG_GLOBAL", "/dev/null"),
					("GIT_CONFIG_SYSTEM", "/dev/null"),
					("XDG_CONFIG_HOME", xdg),
				],
			)
		} else {
			let setting = format!("core.excludesFile={}", excludes.display());
			gta_with_configs(
				&fixture.consumer,
				&["protocol.file.allow=always", &setting],
				&["submodule", "update"],
			)
		};
		assert_success(&update, "module update with global excludes");
		assert_eq!(
			std::fs::read_to_string(module.join("ignored.txt")).unwrap(),
			"upstream\n"
		);
		assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), next);
	}
}

#[test]
fn retained_module_operation_state_is_rejected_before_mount_publication() {
	let fixture = Fixture::new("retained-operation-state");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	git_allow(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);
	let config_before = std::fs::read(module_git_dir.join("config")).unwrap();
	let head_before = std::fs::read(module_git_dir.join("HEAD")).unwrap();
	std::fs::write(
		module_git_dir.join("MERGE_HEAD"),
		format!("{}\n", fixture.old),
	)
	.unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(
		!update.status.success(),
		"an in-progress merge must block attachment"
	);
	assert!(
		stderr(&update).contains("merge is in progress"),
		"unexpected operation-state error: {}",
		stderr(&update)
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("config")).unwrap(),
		config_before,
		"core.worktree must not be published"
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("HEAD")).unwrap(),
		head_before
	);
	assert!(
		!module.join(".git").exists(),
		"the mount marker must remain absent"
	);
	assert!(
		!git_path(&fixture.consumer, "gitana-submodule-update").exists(),
		"attachment intent must not be created"
	);

	std::fs::remove_file(module_git_dir.join("MERGE_HEAD")).unwrap();
	let retry = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&retry, "retry after clearing the operation state");
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn update_can_fetch_an_unadvertised_recorded_commit_by_exact_object_id() {
	let fixture = Fixture::new("exact-oid");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let hidden = fixture.commit_source("hidden\n", "hidden commit");
	fixture.record_superproject_commit(&hidden, "record hidden module commit");
	git_ok(&fixture.source, &["reset", "--hard", &fixture.old]);
	git_ok(&fixture.consumer, &["pull", "--ff-only"]);

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "exact object-id fetch");
	let module = fixture.consumer.join("modules/one");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), hidden);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"hidden\n"
	);
}

#[cfg(unix)]
#[test]
fn ssh_exact_object_fallback_reopens_the_completed_fetch_session() {
	use std::os::unix::fs::PermissionsExt;

	let fixture = Fixture::new("ssh-exact-oid");
	let script = fixture.root.join("fake-ssh.sh");
	let sessions = fixture.root.join("ssh-sessions");
	std::fs::write(
		&script,
		format!(
			"#!/bin/sh\necho session >> \"{}\"\nfor a in \"$@\"; do cmd=\"$a\"; done\neval \"$cmd\"\n",
			sessions.display()
		),
	)
	.unwrap();
	std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
	let source_url = format!("ssh://git@localhost{}", fixture.source.display());
	let alias = "ssh-module:";
	git_ok(
		&fixture.superproject,
		&["config", "-f", ".gitmodules", "submodule.one.url", alias],
	);
	git_ok(&fixture.superproject, &["add", ".gitmodules"]);
	commit(&fixture.superproject, "use ssh submodule source");
	git_ok(
		&fixture.consumer,
		&["pull", "--ff-only", "--recurse-submodules=no"],
	);
	let rewrite_key = format!("url.{source_url}.insteadOf");
	git_ok(&fixture.consumer, &["config", &rewrite_key, alias]);
	let ssh_command = script.to_str().unwrap();
	let environment = [
		("GIT_SSH_COMMAND", ssh_command),
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_SYSTEM", "/dev/null"),
	];
	assert_success(
		&gta_with_environment(
			&fixture.consumer,
			&["submodule", "update", "--init"],
			&environment,
		),
		"initial SSH submodule update",
	);
	let module = fixture.consumer.join("modules/one");
	assert_eq!(
		git(&module, &["config", "--get", "remote.origin.url"]).trim(),
		source_url,
		"the module must retain the effective SSH endpoint"
	);
	git_ok(&fixture.consumer, &["config", "--unset-all", &rewrite_key]);

	let hidden = fixture.commit_source("hidden over ssh\n", "hidden SSH commit");
	fixture.record_superproject_commit(&hidden, "record hidden SSH module commit");
	git_ok(
		&fixture.source,
		&["config", "uploadpack.allowAnySHA1InWant", "true"],
	);
	git_ok(&fixture.source, &["reset", "--hard", &fixture.old]);
	git_ok(
		&fixture.consumer,
		&["pull", "--ff-only", "--recurse-submodules=no"],
	);
	std::fs::write(&sessions, b"").unwrap();

	let update = gta_with_environment(&fixture.consumer, &["submodule", "update"], &environment);
	assert_success(&update, "SSH exact-object fallback");
	assert_eq!(
		std::fs::read_to_string(&sessions).unwrap().lines().count(),
		2,
		"normal fetch and exact-object fallback must use separate SSH sessions"
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), hidden);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"hidden over ssh\n"
	);
}

#[test]
fn an_existing_current_module_still_fetches_advertised_refs() {
	let fixture = Fixture::new("existing-advertised-fetch");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let next = fixture.commit_source("advertised\n", "advertised commit");
	let module = fixture.consumer.join("modules/one");

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "advertised fetch for current module");
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/main"]).trim(),
		next
	);
	assert_eq!(
		git(&module, &["rev-parse", "HEAD"]).trim(),
		fixture.old,
		"fetching advertised refs must not move beyond the recorded gitlink"
	);
}

#[test]
fn an_existing_current_module_reports_an_unavailable_origin() {
	let fixture = Fixture::new("existing-fetch-failure");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let unavailable = fixture.root.join("source-unavailable");
	std::fs::rename(&fixture.source, &unavailable).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert!(
		!update.status.success(),
		"a cached recorded object must not hide fetch failure"
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
}

#[test]
fn an_existing_module_fetches_from_the_first_of_multiple_origin_urls() {
	let fixture = Fixture::new("multiple-module-origins");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let missing = fixture.root.join("missing-second-origin");
	git_ok(
		&module,
		&[
			"config",
			"--add",
			"remote.origin.url",
			missing.to_str().unwrap(),
		],
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "update using first module origin URL");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
}

#[test]
fn an_existing_module_origin_can_come_from_an_included_config() {
	let fixture = Fixture::new("included-module-origin");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let included = fixture.root.join("module-origin.config");
	std::fs::write(
		&included,
		format!(
			"[remote \"origin\"]\n\turl = {}\n",
			fixture.source.display()
		),
	)
	.unwrap();
	git_ok(&module, &["config", "--unset-all", "remote.origin.url"]);
	git_ok(
		&module,
		&["config", "include.path", included.to_str().unwrap()],
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "module origin from included config");
}

#[test]
fn an_existing_module_origin_can_come_from_global_config() {
	let fixture = Fixture::new("global-module-origin");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(&module, &["config", "--unset-all", "remote.origin.url"]);
	let global = fixture.root.join("global.config");
	std::fs::write(
		&global,
		format!(
			"[remote \"origin\"]\n\turl = {}\n[protocol \"file\"]\n\tallow = always\n",
			fixture.source.display()
		),
	)
	.unwrap();
	let environment = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_SYSTEM", "/dev/null"),
	];

	let update = gta_with_environment(&fixture.consumer, &["submodule", "update"], &environment);
	assert_success(&update, "module origin from global config");
}

#[test]
fn command_scope_can_reset_and_redirect_an_existing_module_origin() {
	let fixture = Fixture::new("command-module-origin");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let unavailable = fixture.root.join("unavailable-module-origin");
	git_ok(
		&module,
		&[
			"config",
			"--replace-all",
			"remote.origin.url",
			unavailable.to_str().unwrap(),
		],
	);
	let source = format!("remote.origin.url={}", fixture.source.display());
	let update = gta_with_configs(
		&fixture.consumer,
		&["protocol.file.allow=always", "remote.origin.url=", &source],
		&["submodule", "update"],
	);
	assert_success(&update, "command-scope module origin redirect");
}

#[test]
fn linked_worktrees_keep_module_repositories_in_their_own_git_directory() {
	let fixture = Fixture::new("linked-worktree");
	let linked = fixture.root.join("linked");
	git_ok(
		&fixture.consumer,
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"linked",
			linked.to_str().unwrap(),
		],
	);

	let update = gta(&linked, true, &["submodule", "update", "--init"]);
	assert_success(&update, "linked-worktree update");
	assert_eq!(
		git(&linked.join("modules/one"), &["rev-parse", "HEAD"]).trim(),
		fixture.old
	);
	assert_mount_points_at_per_worktree_repository(&linked);

	let linked_module_git_dir = git_path(&linked, "modules/one");
	let linked_git_dir = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]).trim());
	assert!(
		linked_module_git_dir.starts_with(&linked_git_dir),
		"module repository {} must be under linked git dir {}",
		linked_module_git_dir.display(),
		linked_git_dir.display()
	);
	assert!(
		!fixture.consumer.join(".git/modules/one").exists(),
		"the main worktree must not receive the linked worktree's module repository"
	);

	let next = fixture.commit_source("next\n", "next");
	let module = linked.join("modules/one");
	git_allow(&module, &["fetch", "origin"]);
	git_ok(&module, &["checkout", "-q", &next]);

	let status = gta(&linked, false, &["status"]);
	assert_success(&status, "linked-worktree status after module HEAD move");
	assert_eq!(
		stdout(&status),
		git(&linked, &["status", "--porcelain"]),
		"gta status must resolve the module repository from the linked worktree's git directory"
	);
	let diff = gta(&linked, false, &["diff"]);
	assert_success(&diff, "linked-worktree diff after module HEAD move");
	let diff = stdout(&diff);
	assert!(
		diff.contains(&format!("-Subproject commit {}", fixture.old))
			&& diff.contains(&format!("+Subproject commit {next}")),
		"gta diff must report the moved gitlink: {diff}"
	);
	let modified = gta(&linked, false, &["ls-files", "-m"]);
	assert_success(&modified, "linked-worktree ls-files -m");
	assert_eq!(
		stdout(&modified),
		git(&linked, &["ls-files", "-m"]),
		"ls-files -m must report the moved linked-worktree module"
	);

	let add = gta(&linked, false, &["add", "modules/one"]);
	assert_success(&add, "stage linked-worktree module pointer");
	assert_eq!(
		git(&linked, &["ls-files", "--stage", "modules/one"]).trim(),
		format!("160000 {next} 0\tmodules/one"),
		"gta add must stage the linked worktree module's current HEAD"
	);
}

#[test]
fn pathspecs_are_resolved_from_the_invocation_subdirectory() {
	let fixture = Fixture::new("subdirectory");
	let subdirectory = fixture.consumer.join("modules");
	std::fs::create_dir_all(&subdirectory).unwrap();

	let status = gta(&subdirectory, false, &["submodule", "status", "one"]);
	assert_success(&status, "subdirectory status");
	assert_eq!(stdout(&status), format!("-{} one\n", fixture.old));

	let update = gta(
		&subdirectory,
		true,
		&["submodule", "update", "--init", "one"],
	);
	assert_success(&update, "subdirectory update");
	assert_eq!(
		stdout(&update),
		format!("Submodule path 'one': checked out '{}'\n", fixture.old)
	);
	assert_eq!(
		git(&subdirectory.join("one"), &["rev-parse", "HEAD"]).trim(),
		fixture.old
	);
}

#[test]
fn bare_local_submodule_urls_resolve_from_the_worktree_root() {
	let fixture = Fixture::new("bare-local-root");
	let local_source = fixture.consumer.join("local-src");
	let clone = Command::new("git")
		.args(["clone", "-q"])
		.arg(&fixture.source)
		.arg(&local_source)
		.output()
		.expect("clone local module source");
	assert!(
		clone.status.success(),
		"clone local source: {}",
		stderr(&clone)
	);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			"local-src",
		],
	);
	let nested = fixture.consumer.join("nested");
	std::fs::create_dir(&nested).unwrap();

	let update = gta(
		&nested,
		true,
		&["submodule", "update", "--init", "../modules/one"],
	);
	assert_success(&update, "nested bare-local submodule update");
	let module = fixture.consumer.join("modules/one");
	assert_eq!(
		git(&fixture.consumer, &["config", "--get", "submodule.one.url"]).trim(),
		"local-src",
		"the superproject keeps the configured spelling"
	);
	assert_eq!(
		git(&module, &["config", "--get", "remote.origin.url"]).trim(),
		std::fs::canonicalize(&local_source)
			.unwrap()
			.to_str()
			.unwrap(),
		"the module origin is stable outside the invocation directory"
	);

	std::fs::write(local_source.join("file.txt"), b"root based\n").unwrap();
	git_ok(&local_source, &["add", "file.txt"]);
	commit(&local_source, "root based");
	let next = git(&local_source, &["rev-parse", "HEAD"]).trim().to_owned();
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{next},modules/one"),
		],
	);
	let update = gta(&nested, true, &["submodule", "update", "../modules/one"]);
	assert_success(&update, "nested bare-local submodule refresh");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), next);
}

#[test]
fn existing_relative_module_origins_resolve_from_the_module_worktree() {
	let fixture = Fixture::new("relative-existing-module-origin");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(
		&module,
		&[
			"config",
			"--replace-all",
			"remote.origin.url",
			"../../../source",
		],
	);

	let next = fixture.commit_source("module relative\n", "module relative");
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{next},modules/one"),
		],
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "update with a module-relative origin");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), next);
}

#[test]
fn dot_branch_remote_resolves_relative_urls_from_the_superproject_root() {
	let fixture = Fixture::new("dot-branch-remote");
	git_ok(&fixture.consumer, &["config", "branch.main.remote", "."]);
	let nested = fixture.consumer.join("nested");
	std::fs::create_dir(&nested).unwrap();

	let update = gta(
		&nested,
		true,
		&["submodule", "update", "--init", "../modules/one"],
	);
	assert_success(&update, "dot-remote submodule update");
	let expected = std::fs::canonicalize(&fixture.source).unwrap();
	assert_eq!(
		git(&fixture.consumer, &["config", "--get", "submodule.one.url"]).trim(),
		expected.to_str().unwrap()
	);
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn missing_origin_url_falls_back_to_the_superproject_for_relative_urls() {
	let fixture = Fixture::new("missing-origin-relative-init");
	git_ok(
		&fixture.consumer,
		&["config", "--remove-section", "remote.origin"],
	);

	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert_success(&init, "relative init without an origin URL");
	assert_eq!(
		stderr(&init).matches("warning: could not look up configuration 'remote.origin.url'. Assuming this repository is its own authoritative upstream.").count(),
		1,
		"the Git-compatible fallback warning is emitted once: {}",
		stderr(&init)
	);
	assert_eq!(
		git(&fixture.consumer, &["config", "--get", "submodule.one.url"]).trim(),
		std::fs::canonicalize(&fixture.source)
			.unwrap()
			.to_str()
			.unwrap()
	);
}

#[test]
fn empty_origin_url_falls_back_to_the_superproject_for_relative_urls() {
	let fixture = Fixture::new("empty-origin-relative-init");
	git_ok(
		&fixture.consumer,
		&["config", "--replace-all", "remote.origin.url", ""],
	);

	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert_success(&init, "relative init with an empty origin URL");
	assert!(
		stderr(&init).contains("warning: could not look up configuration 'remote.origin.url'. Assuming this repository is its own authoritative upstream."),
		"the empty URL uses the missing-remote fallback: {}",
		stderr(&init)
	);
	assert_eq!(
		git(&fixture.consumer, &["config", "--get", "submodule.one.url"]).trim(),
		std::fs::canonicalize(&fixture.source)
			.unwrap()
			.to_str()
			.unwrap()
	);
}

#[test]
fn update_init_uses_the_superproject_when_the_origin_url_is_missing() {
	let fixture = Fixture::new("missing-origin-relative-update");
	git_ok(
		&fixture.consumer,
		&["config", "--remove-section", "remote.origin"],
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "relative update --init without an origin URL");
	assert!(
		stderr(&update).contains("warning: could not look up configuration 'remote.origin.url'. Assuming this repository is its own authoritative upstream."),
		"the fallback must be reported: {}",
		stderr(&update)
	);
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
}

#[test]
fn missing_named_branch_remote_url_falls_back_to_the_superproject() {
	let fixture = Fixture::new("missing-named-remote-relative-init");
	git_ok(
		&fixture.consumer,
		&["config", "--remove-section", "remote.origin"],
	);
	git_ok(
		&fixture.consumer,
		&["config", "branch.main.remote", "missing"],
	);

	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert_success(&init, "relative init with a missing named remote URL");
	assert!(
		stderr(&init).contains("warning: could not look up configuration 'remote.missing.url'. Assuming this repository is its own authoritative upstream."),
		"the warning must identify the selected remote key: {}",
		stderr(&init)
	);
	assert_eq!(
		git(&fixture.consumer, &["config", "--get", "submodule.one.url"]).trim(),
		std::fs::canonicalize(&fixture.source)
			.unwrap()
			.to_str()
			.unwrap()
	);
}

#[test]
fn authoritative_superproject_warning_is_not_repeated_per_module() {
	let fixture = Fixture::new("missing-origin-relative-multiple");
	add_second_module_mapping(&fixture);
	git_ok(
		&fixture.consumer,
		&["config", "--remove-section", "remote.origin"],
	);

	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert_success(&init, "relative init for multiple modules without origin");
	assert_eq!(
		stderr(&init)
			.matches("Assuming this repository is its own authoritative upstream.")
			.count(),
		1,
		"one base-resolution notice must cover every selected module: {}",
		stderr(&init)
	);
	assert_eq!(
		git(&fixture.consumer, &["config", "--get", "submodule.one.url"]).trim(),
		git(&fixture.consumer, &["config", "--get", "submodule.two.url"]).trim()
	);
}

#[test]
fn update_reuses_a_deinitialized_repository_and_repopulates_its_mount() {
	let fixture = Fixture::new("deinitialized");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);
	assert!(module_git_dir.exists(), "Git retains the module repository");
	assert!(!fixture.consumer.join("modules/one/.git").exists());

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "reattach deinitialized repository");
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
}

#[test]
fn retained_attachment_uses_the_module_origin_not_the_new_declaration_url() {
	let fixture = Fixture::new("retained-module-origin");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			"unsupported://replacement",
		],
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "reattach through the retained module origin");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
}

#[test]
fn module_scoped_recovery_uses_the_retained_origin_while_offline() {
	let fixture = Fixture::new("module-scoped-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let module_origin = git(&module, &["config", "--get", "remote.origin.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);
	git_ok(
		&fixture.consumer,
		&["config", "submodule.one.url", "unsupported://replacement"],
	);
	let control = write_v4_recovery_intent(
		&fixture,
		"one",
		"modules/one",
		&fixture.old,
		&module_origin,
		"module",
	);
	let unavailable = fixture.root.join("source-offline");
	std::fs::rename(&fixture.source, &unavailable).unwrap();

	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(
		&recovered,
		"recover a retained attachment from its module-scoped intent",
	);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"old\n"
	);
	assert!(
		!control.exists(),
		"completed recovery must clear its intent"
	);
}

#[test]
fn module_scoped_recovery_rejects_a_changed_module_origin() {
	let fixture = Fixture::new("module-source-rebinding");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let module_origin = git(&module, &["config", "--get", "remote.origin.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);
	let control = write_v4_recovery_intent(
		&fixture,
		"one",
		"modules/one",
		&fixture.old,
		&module_origin,
		"module",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	git_ok(
		&module_git_dir,
		&[
			"config",
			"--replace-all",
			"remote.origin.url",
			fixture.superproject.to_str().unwrap(),
		],
	);

	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(
		!recovered.status.success(),
		"changed module origin must fail"
	);
	assert!(
		stderr(&recovered).contains("unfinished staging source does not match 'one'"),
		"unexpected error: {}",
		stderr(&recovered)
	);
	assert!(control.join("intent.json").is_file());
}

#[test]
fn published_repository_recovery_finishes_without_its_remote() {
	let fixture = Fixture::new("published-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let source_url = git(&fixture.consumer, &["config", "--get", "submodule.one.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);

	let control = write_v3_recovery_intent(&fixture, "one", "modules/one", &fixture.old, &source_url);
	let unavailable = fixture.root.join("source-offline");
	std::fs::rename(&fixture.source, &unavailable).unwrap();

	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(
		&recovered,
		"recover published repository without its remote",
	);
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
	assert!(
		!control.exists(),
		"the intent is cleared only after checkout and HEAD publication complete"
	);
}

#[test]
fn unpublished_v1_recovery_is_upgraded_and_reprepared() {
	let fixture = Fixture::new("unpublished-v1-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let source_url = git(&fixture.consumer, &["config", "--get", "submodule.one.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);

	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let control = write_v1_recovery_intent(&fixture, "one", "modules/one", &fixture.old, &source_url);
	std::fs::rename(&module_git_dir, control.join("repository")).unwrap();

	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&recovered, "upgrade and retry unpublished v1 recovery");
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
	assert!(
		module_git_dir.is_dir(),
		"the repository must be republished"
	);
	assert!(
		!control.exists(),
		"completed recovery must clear its intent"
	);
}

#[test]
fn a_later_published_recovery_completes_before_earlier_index_entries() {
	let fixture = Fixture::new("later-published-recovery");
	add_second_module_mapping(&fixture);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--init", "modules/two"],
		),
		"materialize later module",
	);
	let source_url = git(&fixture.consumer, &["config", "--get", "submodule.two.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/two"],
	);
	let control = write_v2_recovery_intent(&fixture, "two", "modules/two", &fixture.old, &source_url);

	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(
		&recovered,
		"recover later published module before earlier module",
	);
	assert_recovery_precedes_normal_update(&fixture, &recovered, &control);
}

#[test]
fn a_later_unpublished_recovery_retries_before_earlier_index_entries() {
	let fixture = Fixture::new("later-unpublished-recovery");
	add_second_module_mapping(&fixture);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--init", "modules/two"],
		),
		"materialize later module",
	);
	let source_url = git(&fixture.consumer, &["config", "--get", "submodule.two.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/two"],
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/two");
	let control = write_v2_recovery_intent(&fixture, "two", "modules/two", &fixture.old, &source_url);
	std::fs::rename(&module_git_dir, control.join("repository")).unwrap();

	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(
		&recovered,
		"retry later unpublished module before earlier module",
	);
	assert_recovery_precedes_normal_update(&fixture, &recovered, &control);
}

#[test]
fn retained_mount_marker_recovery_forces_checkout_before_clearing_intent() {
	let fixture = Fixture::new("retained-marker-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let marker = std::fs::read(fixture.consumer.join("modules/one/.git")).unwrap();
	let source_url = git(&fixture.consumer, &["config", "--get", "submodule.one.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);

	let control = git_path(&fixture.consumer, "gitana-submodule-update");
	std::fs::create_dir_all(&control).unwrap();
	let fingerprint = Sha256::digest(gitana_remote::redact_password(&source_url).as_bytes());
	let fingerprint = fingerprint
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>();
	std::fs::write(
		control.join("intent.json"),
		format!(
			"{{\"version\":2,\"name\":\"one\",\"path\":\"modules/one\",\"recorded\":\"{}\",\"source_fingerprint\":\"{fingerprint}\"}}",
			fixture.old
		),
	)
	.unwrap();
	let mount = fixture.consumer.join("modules/one");
	std::fs::create_dir_all(&mount).unwrap();
	std::fs::write(mount.join(".git"), marker).unwrap();
	assert!(!mount.join("file.txt").exists());

	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&recovered, "recover retained marker attachment");
	assert_eq!(
		std::fs::read_to_string(mount.join("file.txt")).unwrap(),
		"old\n"
	);
	assert!(
		!control.exists(),
		"recovery intent must remain until checkout and HEAD are durable"
	);
}

#[test]
fn recovery_population_preserves_conflicting_mount_content_until_a_clean_retry() {
	let fixture = Fixture::new("recovery-no-clobber");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let marker = std::fs::read(fixture.consumer.join("modules/one/.git")).unwrap();
	let source_url = git(&fixture.consumer, &["config", "--get", "submodule.one.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);
	let control = write_v2_recovery_intent(&fixture, "one", "modules/one", &fixture.old, &source_url);
	let mount = fixture.consumer.join("modules/one");
	std::fs::create_dir_all(&mount).unwrap();
	std::fs::write(mount.join(".git"), marker).unwrap();
	std::fs::write(mount.join("file.txt"), "concurrent user data\n").unwrap();

	let blocked = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(
		!blocked.status.success(),
		"conflicting population must fail"
	);
	assert_eq!(
		std::fs::read_to_string(mount.join("file.txt")).unwrap(),
		"concurrent user data\n"
	);
	assert!(
		control.exists(),
		"recovery intent must remain after conflict"
	);

	std::fs::remove_file(mount.join("file.txt")).unwrap();
	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&recovered, "clean recovery retry");
	assert_eq!(
		std::fs::read_to_string(mount.join("file.txt")).unwrap(),
		"old\n"
	);
	assert!(
		!control.exists(),
		"successful retry must clear recovery intent"
	);
}

#[test]
fn an_already_current_mount_preserves_a_tracked_deletion() {
	let fixture = Fixture::new("already-current-deletion");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let tracked = fixture.consumer.join("modules/one/file.txt");
	std::fs::remove_file(&tracked).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "same-commit update");
	assert!(
		!tracked.exists(),
		"a completed mount must not be mistaken for interrupted attachment"
	);
}

#[test]
fn a_module_gitdir_name_ending_in_space_remains_a_valid_mount() {
	let fixture = Fixture::new("trailing-space-name");
	let modules = fixture.consumer.join(".gitmodules");
	let declaration = std::fs::read_to_string(&modules)
		.unwrap()
		.replace("[submodule \"one\"]", "[submodule \"one \"]");
	std::fs::write(&modules, declaration).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "trailing-space module update");
	let marker = std::fs::read_to_string(fixture.consumer.join("modules/one/.git")).unwrap();
	assert!(
		marker.ends_with("modules/one \n"),
		"canonical marker must retain the trailing space: {marker:?}"
	);
	let status = gta(&fixture.consumer, false, &["submodule", "status"]);
	assert_success(&status, "status with trailing-space module name");
	assert_eq!(stdout(&status), format!(" {} modules/one\n", fixture.old));
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update"]),
		"repeat update with trailing-space module name",
	);
}

#[test]
fn a_foreign_mount_is_rejected_before_module_config_is_changed() {
	let fixture = Fixture::new("foreign-mount");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	git_ok(
		&module_git_dir,
		&["config", "--file", "config", "core.worktree", "sentinel"],
	);
	std::fs::write(
		fixture.consumer.join("modules/one/.git"),
		b"gitdir: ../../foreign\n",
	)
	.unwrap();
	let status = gta(&fixture.consumer, false, &["submodule", "status"]);
	assert!(
		!status.status.success(),
		"status must not hide a foreign mount"
	);

	let rejected = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert!(!rejected.status.success(), "foreign mount must fail");
	assert!(
		stderr(&rejected).contains("foreign or non-empty content"),
		"unexpected error: {}",
		stderr(&rejected)
	);
	assert!(
		std::fs::read_to_string(module_git_dir.join("config"))
			.unwrap()
			.contains("worktree = sentinel"),
		"foreign-mount rejection must leave core.worktree untouched"
	);
}

#[cfg(unix)]
#[test]
fn status_rejects_symlinked_mount_components() {
	use std::os::unix::fs::symlink;

	for (tag, symlink_final_mount) in [
		("status-symlinked-mount-parent", false),
		("status-symlinked-final-mount", true),
	] {
		let fixture = Fixture::new(tag);
		assert_success(
			&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
			"initial update",
		);
		if symlink_final_mount {
			std::fs::rename(
				fixture.consumer.join("modules/one"),
				fixture.consumer.join("modules/actual"),
			)
			.unwrap();
			symlink("actual", fixture.consumer.join("modules/one")).unwrap();
		} else {
			std::fs::rename(
				fixture.consumer.join("modules"),
				fixture.consumer.join("actual"),
			)
			.unwrap();
			symlink("actual", fixture.consumer.join("modules")).unwrap();
		}

		let status = gta(&fixture.consumer, false, &["submodule", "status"]);
		assert!(!status.status.success(), "symlinked mount must be refused");
		assert!(
			stderr(&status).contains("foreign or non-empty content"),
			"unexpected error: {}",
			stderr(&status)
		);
	}
}

#[test]
fn update_preserves_an_equivalent_existing_marker() {
	let fixture = Fixture::new("atomic-marker-rewrite");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let mount = fixture.consumer.join("modules/one");
	let marker = mount.join(".git");
	let module_git_dir = std::fs::canonicalize(git_path(&fixture.consumer, "modules/one")).unwrap();
	let equivalent = format!("gitdir: {}\n", module_git_dir.display());
	std::fs::write(&marker, &equivalent).unwrap();
	#[cfg(unix)]
	let before = std::fs::symlink_metadata(&marker).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "equivalent marker update");
	assert_eq!(
		std::fs::read_to_string(&marker).unwrap(),
		equivalent,
		"an accepted marker must retain its exact spelling"
	);
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		assert_eq!(
			std::fs::symlink_metadata(&marker).unwrap().ino(),
			before.ino()
		);
	}
}

#[test]
fn update_dispatches_sha256_superprojects_and_modules() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let fixture = Fixture::new_with_format("sha256", Some("sha256"));
	assert_eq!(fixture.old.len(), 64);
	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "SHA-256 update");
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
	assert_eq!(
		stdout(&gta(&fixture.consumer, false, &["submodule", "status"])),
		format!(" {} modules/one\n", fixture.old)
	);
}

#[test]
fn unsupported_update_strategy_fails_before_init_or_repository_creation() {
	let fixture = Fixture::new("unsupported-strategy");
	let modules = fixture.consumer.join(".gitmodules");
	let mut declaration = std::fs::read_to_string(&modules).unwrap();
	declaration.push_str("\tupdate = rebase\n");
	std::fs::write(&modules, declaration).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(!update.status.success());
	assert!(
		stderr(&update).contains("unsupported submodule update strategy 'rebase'"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert!(
		!std::fs::read_to_string(fixture.consumer.join(".git/config"))
			.unwrap()
			.contains("[submodule \"one\"]"),
		"preflight failure must precede init"
	);
	assert!(!git_path(&fixture.consumer, "modules/one").exists());
}

#[test]
fn standalone_init_rejects_every_unsupported_declaration_strategy_before_config_mutation() {
	for (tag, strategy) in [
		("init-rebase", "rebase"),
		("init-merge", "merge"),
		("init-custom", "!echo nope"),
	] {
		let fixture = Fixture::new(tag);
		let modules = fixture.consumer.join(".gitmodules");
		let mut declaration = std::fs::read_to_string(&modules).unwrap();
		declaration.push_str(&format!("\tupdate = {strategy}\n"));
		std::fs::write(&modules, declaration).unwrap();
		git_ok(
			&fixture.consumer,
			&["config", "submodule.one.update", "checkout"],
		);
		let config = fixture.consumer.join(".git/config");
		let before = std::fs::read(&config).unwrap();

		let init = gta(&fixture.consumer, false, &["submodule", "init"]);
		assert!(!init.status.success(), "init must reject {strategy}");
		assert!(
			stderr(&init).contains("unsupported submodule update strategy"),
			"unexpected init error for {strategy}: {}",
			stderr(&init)
		);
		assert_eq!(
			std::fs::read(&config).unwrap(),
			before,
			"strategy rejection must precede the config transaction"
		);
		assert!(!git_path(&fixture.consumer, "modules/one").exists());
	}
}

#[test]
fn standalone_init_rejects_unsupported_effective_strategies_before_config_mutation() {
	for (tag, strategy) in [
		("configured-rebase", "rebase"),
		("configured-merge", "merge"),
		("configured-custom", "!echo nope"),
	] {
		let fixture = Fixture::new(tag);
		git_ok(
			&fixture.consumer,
			&["config", "submodule.one.update", strategy],
		);
		let config = fixture.consumer.join(".git/config");
		let before = std::fs::read(&config).unwrap();

		let init = gta(&fixture.consumer, false, &["submodule", "init"]);
		assert!(!init.status.success(), "init must reject {strategy}");
		assert!(
			stderr(&init).contains("unsupported submodule update strategy"),
			"unexpected init error for {strategy}: {}",
			stderr(&init)
		);
		assert_eq!(std::fs::read(&config).unwrap(), before);
	}

	let fixture = Fixture::new("command-configured-rebase");
	let config = fixture.consumer.join(".git/config");
	let before = std::fs::read(&config).unwrap();
	let init = gta_with_config(
		&fixture.consumer,
		"submodule.one.update=rebase",
		&["submodule", "init"],
	);
	assert!(
		!init.status.success(),
		"command-scope rebase must fail init"
	);
	assert!(stderr(&init).contains("unsupported submodule update strategy"));
	assert_eq!(std::fs::read(&config).unwrap(), before);
}

#[test]
fn standalone_init_rejects_a_relative_url_that_escapes_an_unanchored_base() {
	let fixture = Fixture::new("relative-url-escape");
	git_ok(&fixture.consumer, &["config", "remote.origin.url", "foo"]);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			"../../bar",
		],
	);
	let config = fixture.consumer.join(".git/config");
	let before = std::fs::read(&config).unwrap();

	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert!(
		!init.status.success(),
		"escaping relative URL must fail init"
	);
	assert!(
		stderr(&init).contains("escapes its superproject remote base"),
		"unexpected relative URL error: {}",
		stderr(&init)
	);
	assert_eq!(
		std::fs::read(&config).unwrap(),
		before,
		"relative URL validation must precede the config transaction"
	);
}

#[test]
fn standalone_init_strips_one_parent_past_an_absolute_local_root_like_git() {
	let fixture = Fixture::new("relative-url-local-root");
	git_ok(
		&fixture.consumer,
		&["config", "remote.origin.url", "/srv/team/super"],
	);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			"../../../../sub",
		],
	);

	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert_success(&init, "one-parent-past-root relative URL init");
	assert_eq!(
		git(&fixture.consumer, &["config", "--get", "submodule.one.url"]).trim(),
		"sub"
	);
}

#[test]
fn no_op_init_does_not_acquire_the_repository_config_lock() {
	let initialized = Fixture::new("no-op-init-initialized");
	assert_success(
		&gta(&initialized.consumer, false, &["submodule", "init"]),
		"initial init",
	);
	let config = initialized.consumer.join(".git/config");
	let before = std::fs::read(&config).unwrap();
	let before_metadata = std::fs::metadata(&config).unwrap();
	std::fs::write(initialized.consumer.join(".git/config.lock"), b"held").unwrap();

	let repeated = gta(&initialized.consumer, false, &["submodule", "init"]);
	assert_success(&repeated, "already-initialized no-op init");
	assert_eq!(std::fs::read(&config).unwrap(), before);
	let after_metadata = std::fs::metadata(&config).unwrap();
	assert_eq!(after_metadata.permissions(), before_metadata.permissions());
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		assert_eq!(after_metadata.ino(), before_metadata.ino());
	}

	let empty = Fixture::new("no-op-init-empty");
	git_ok(
		&empty.consumer,
		&["update-index", "--force-remove", "modules/one"],
	);
	let empty_config = empty.consumer.join(".git/config");
	let empty_before = std::fs::read(&empty_config).unwrap();
	std::fs::write(empty.consumer.join(".git/config.lock"), b"held").unwrap();
	let no_gitlinks = gta(&empty.consumer, false, &["submodule", "init"]);
	assert_success(&no_gitlinks, "no-gitlink no-op init");
	assert_eq!(std::fs::read(&empty_config).unwrap(), empty_before);
}

#[test]
fn normalization_equivalent_names_fail_before_initialization() {
	let fixture = Fixture::new("unicode-collision");
	for path in ["modules/nfc", "modules/nfd"] {
		git_ok(
			&fixture.consumer,
			&[
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("160000,{},{}", fixture.old, path),
			],
		);
	}
	let modules = fixture.consumer.join(".gitmodules");
	let mut declarations = std::fs::read_to_string(&modules).unwrap();
	declarations.push_str(
		"[submodule \"caf\u{e9}\"]\n\tpath = modules/nfc\n\turl = ../source\n\
		 [submodule \"cafe\u{301}\"]\n\tpath = modules/nfd\n\turl = ../source\n",
	);
	std::fs::write(modules, declarations).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(!update.status.success(), "Unicode aliases must be refused");
	assert!(
		stderr(&update).contains("ambiguous .gitmodules declarations"),
		"unexpected error: {}",
		stderr(&update)
	);
	let local_config = std::fs::read_to_string(fixture.consumer.join(".git/config")).unwrap();
	assert!(
		!local_config.contains("[submodule "),
		"namespace rejection must precede init"
	);
	assert!(!git_path(&fixture.consumer, "modules/caf\u{e9}").exists());
	assert!(!git_path(&fixture.consumer, "modules/cafe\u{301}").exists());
}

#[test]
fn ntfs_dot_git_aliases_fail_before_initialization() {
	for (tag, from, to) in [
		(
			"ntfs-name-dot-space",
			"[submodule \"one\"]",
			"[submodule \".GiT \"]",
		),
		(
			"ntfs-name-short",
			"[submodule \"one\"]",
			"[submodule \"GIT~1.\"]",
		),
		(
			"ntfs-path-dot-space",
			"\tpath = modules/one",
			"\tpath = \".git /one\"",
		),
		(
			"ntfs-path-short",
			"\tpath = modules/one",
			"\tpath = a/GIT~1./one",
		),
	] {
		let fixture = Fixture::new(tag);
		let modules = fixture.consumer.join(".gitmodules");
		let declaration = std::fs::read_to_string(&modules).unwrap().replace(from, to);
		std::fs::write(&modules, declaration).unwrap();
		let config = fixture.consumer.join(".git/config");
		let before = std::fs::read(&config).unwrap();

		let init = gta(&fixture.consumer, false, &["submodule", "init"]);
		assert!(!init.status.success(), "NTFS .git alias must be refused");
		assert_eq!(
			std::fs::read(&config).unwrap(),
			before,
			"alias rejection must precede initialization"
		);
	}
}

#[test]
fn none_update_strategy_initializes_but_does_not_materialize() {
	let fixture = Fixture::new("none-strategy");
	let modules = fixture.consumer.join(".gitmodules");
	let mut declaration = std::fs::read_to_string(&modules).unwrap();
	declaration.push_str("\tupdate = none\n");
	std::fs::write(&modules, declaration).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "none-strategy update");
	assert!(stderr(&update).contains("Skipping submodule 'modules/one'"));
	assert_eq!(
		git(
			&fixture.consumer,
			&["config", "--get", "submodule.one.update"]
		)
		.trim(),
		"none"
	);
	assert!(!git_path(&fixture.consumer, "modules/one").exists());
}

#[test]
fn declaration_none_strategy_is_honored_without_init() {
	let fixture = Fixture::new("declared-none-without-init");
	let modules = fixture.consumer.join(".gitmodules");
	let mut declaration = std::fs::read_to_string(&modules).unwrap();
	declaration.push_str("\tupdate = none\n");
	std::fs::write(&modules, declaration).unwrap();
	git_ok(
		&fixture.consumer,
		&["config", "submodule.one.url", "../source"],
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "declared none update without init");
	assert!(stderr(&update).contains("Skipping submodule 'modules/one'"));
	assert!(
		!std::fs::read_to_string(fixture.consumer.join(".git/config"))
			.unwrap()
			.contains("update ="),
		"update without --init must not persist the declaration strategy"
	);
	assert!(!git_path(&fixture.consumer, "modules/one").exists());
	assert!(!fixture.consumer.join("modules/one/.git").exists());
}

#[cfg(unix)]
#[test]
fn update_init_rejects_mount_and_module_namespace_symlinks_before_config_mutation() {
	use std::os::unix::fs::symlink;

	for (tag, module_namespace) in [
		("preflight-mount-namespace", false),
		("preflight-module-namespace", true),
	] {
		let fixture = Fixture::new(tag);
		let config = fixture.consumer.join(".git/config");
		let before = std::fs::read(&config).unwrap();
		let foreign = fixture.root.join("foreign-namespace");
		std::fs::create_dir(&foreign).unwrap();
		if module_namespace {
			symlink(&foreign, fixture.consumer.join(".git/modules")).unwrap();
		} else {
			std::fs::remove_dir_all(fixture.consumer.join("modules")).unwrap();
			symlink(&foreign, fixture.consumer.join("modules")).unwrap();
		}

		let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
		assert!(!update.status.success(), "symlinked namespace must fail");
		assert_eq!(
			std::fs::read(&config).unwrap(),
			before,
			"namespace rejection must precede --init config mutation"
		);
		assert!(
			!foreign.join("one").exists(),
			"namespace validation must not follow or populate the symlink target"
		);
	}
}

#[cfg(unix)]
#[test]
fn submodule_init_preserves_symlinked_config_and_target_mode() {
	use std::os::unix::fs::PermissionsExt;

	let fixture = Fixture::new("symlinked-init-config");
	let config = fixture.consumer.join(".git/config");
	let target = fixture.root.join("repository-config");
	std::fs::rename(&config, &target).unwrap();
	std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
	std::os::unix::fs::symlink(&target, &config).unwrap();

	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert_success(&init, "submodule init through symlinked config");
	assert!(
		std::fs::symlink_metadata(&config)
			.unwrap()
			.file_type()
			.is_symlink(),
		"init must preserve the config symlink"
	);
	let rendered = std::fs::read_to_string(&target).unwrap();
	assert!(rendered.contains("[submodule \"one\"]"));
	assert!(rendered.contains("active = true"));
	assert_eq!(
		std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
		0o600
	);
}

#[cfg(unix)]
#[test]
fn submodule_update_preserves_module_config_symlink_and_target_mode() {
	use std::os::unix::fs::PermissionsExt;

	let fixture = Fixture::new("symlinked-module-config");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let config = module_git_dir.join("config");
	let target = fixture.root.join("module-repository-config");
	std::fs::rename(&config, &target).unwrap();
	std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
	std::os::unix::fs::symlink(&target, &config).unwrap();

	let status = gta(&fixture.consumer, false, &["submodule", "status"]);
	assert_success(&status, "status through symlinked module config");
	assert_eq!(stdout(&status), format!(" {} modules/one\n", fixture.old));

	let update = gta(&fixture.consumer, true, &["submodule", "update"]);
	assert_success(&update, "update through symlinked module config");
	assert!(
		std::fs::symlink_metadata(&config)
			.unwrap()
			.file_type()
			.is_symlink(),
		"update must preserve the module config symlink"
	);
	assert!(
		std::fs::read_to_string(&target)
			.unwrap()
			.contains("worktree =")
	);
	assert_eq!(
		std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
		0o600
	);
}

#[test]
fn a_later_failure_reports_and_preserves_the_completed_module_prefix() {
	let fixture = Fixture::new("partial-prefix");
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{},modules/two", fixture.old),
		],
	);
	let modules = fixture.consumer.join(".gitmodules");
	let mut declarations = std::fs::read_to_string(&modules).unwrap();
	declarations.push_str("[submodule \"two\"]\n\tpath = modules/two\n\turl = ../missing-source\n");
	std::fs::write(modules, declarations).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(
		!update.status.success(),
		"the missing second source must fail"
	);
	assert_eq!(
		stdout(&update),
		format!(
			"Submodule path 'modules/one': checked out '{}'\n",
			fixture.old
		),
		"the successfully completed prefix is rendered before the error"
	);
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
	assert!(!git_path(&fixture.consumer, "modules/two").exists());
}

#[test]
fn a_post_init_lock_failure_still_reports_the_completed_initialization() {
	let fixture = Fixture::new("post-init-lock");
	let lock_path = fixture.consumer.join(".git/gitana-submodule-update.lock");
	let lock = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(false)
		.open(&lock_path)
		.unwrap();
	std::fs::File::try_lock(&lock).unwrap();

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(!update.status.success(), "the held update lock must fail");
	assert!(
		stderr(&update).contains("Submodule 'one'"),
		"completed initialization must still be reported: {}",
		stderr(&update)
	);
	assert!(stderr(&update).contains("already running"));
	assert_eq!(
		git(
			&fixture.consumer,
			&["config", "--get", "submodule.one.active"]
		)
		.trim(),
		"true"
	);
	assert!(!git_path(&fixture.consumer, "modules/one").exists());
}

#[cfg(unix)]
#[test]
fn update_init_rejects_a_non_utf8_pointer_before_any_mutation() {
	use std::ffi::OsString;
	use std::os::unix::ffi::{OsStrExt, OsStringExt};

	let fixture = Fixture::new("non-utf8-pointer");
	let old_git_dir = fixture.consumer.join(".git");
	let git_dir = fixture
		.root
		.join(OsString::from_vec(b"consumer-git-\xff".to_vec()));
	if let Err(error) = std::fs::rename(&old_git_dir, &git_dir) {
		eprintln!("skipping filesystem without non-UTF-8 names: {error}");
		return;
	}
	let mut gitfile = b"gitdir: ".to_vec();
	gitfile.extend_from_slice(git_dir.as_os_str().as_bytes());
	gitfile.push(b'\n');
	std::fs::write(&old_git_dir, gitfile).unwrap();

	let config = git_dir.join("config");
	let before = std::fs::read(&config).unwrap();
	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(!update.status.success(), "non-UTF-8 pointer must fail");
	assert!(
		stderr(&update).contains("not representable as UTF-8"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert_eq!(
		std::fs::read(&config).unwrap(),
		before,
		"pointer rejection must precede initialization"
	);
	assert!(!git_dir.join("modules/one").exists());
	assert!(!git_dir.join("gitana-submodule-update").exists());
	assert!(!fixture.consumer.join("modules/one/.git").exists());
}

struct Fixture {
	root: PathBuf,
	source: PathBuf,
	superproject: PathBuf,
	consumer: PathBuf,
	old: String,
}

impl Fixture {
	fn new(tag: &str) -> Self {
		Self::new_with_format(tag, None)
	}

	fn new_with_format(tag: &str, object_format: Option<&str>) -> Self {
		let root = unique_tmp(tag);
		let source = root.join("source");
		let superproject = root.join("super");
		let consumer = root.join("consumer");
		std::fs::create_dir_all(&source).unwrap();
		std::fs::create_dir_all(&superproject).unwrap();

		init_repository(&source, object_format);
		std::fs::write(source.join("file.txt"), b"old\n").unwrap();
		git_ok(&source, &["add", "file.txt"]);
		commit(&source, "old");
		let old = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();

		init_repository(&superproject, object_format);
		std::fs::write(superproject.join("root.txt"), b"root\n").unwrap();
		git_ok(&superproject, &["add", "root.txt"]);
		commit(&superproject, "root");
		git_allow(
			&superproject,
			&[
				"submodule",
				"add",
				"--name",
				"one",
				"../source",
				"modules/one",
			],
		);
		commit(&superproject, "add submodule");
		let clone = Command::new("git")
			.args(["clone", "-q", "--no-recurse-submodules"])
			.arg(&superproject)
			.arg(&consumer)
			.output()
			.expect("clone superproject");
		assert!(
			clone.status.success(),
			"clone superproject: {}",
			String::from_utf8_lossy(&clone.stderr)
		);

		Self {
			root,
			source,
			superproject,
			consumer,
			old,
		}
	}

	fn commit_source(&self, contents: &str, message: &str) -> String {
		std::fs::write(self.source.join("file.txt"), contents).unwrap();
		git_ok(&self.source, &["add", "file.txt"]);
		commit(&self.source, message);
		git(&self.source, &["rev-parse", "HEAD"]).trim().to_owned()
	}

	fn record_superproject_commit(&self, oid: &str, message: &str) {
		let module = self.superproject.join("modules/one");
		git_allow(&module, &["fetch", "origin"]);
		git_ok(&module, &["checkout", "-q", oid]);
		git_ok(&self.superproject, &["add", "modules/one"]);
		commit(&self.superproject, message);
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.root);
	}
}

fn add_second_module_mapping(fixture: &Fixture) {
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{},modules/two", fixture.old),
		],
	);
	let modules = fixture.consumer.join(".gitmodules");
	let mut declarations = std::fs::read_to_string(&modules).unwrap();
	declarations.push_str("[submodule \"two\"]\n\tpath = modules/two\n\turl = ../source\n");
	std::fs::write(modules, declarations).unwrap();
}

fn write_v2_recovery_intent(
	fixture: &Fixture,
	name: &str,
	path: &str,
	recorded: &str,
	source_url: &str,
) -> PathBuf {
	let control = git_path(&fixture.consumer, "gitana-submodule-update");
	std::fs::create_dir_all(&control).unwrap();
	let fingerprint = Sha256::digest(gitana_remote::redact_password(source_url).as_bytes());
	let fingerprint = fingerprint
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>();
	std::fs::write(
		control.join("intent.json"),
		format!(
			"{{\"version\":2,\"name\":\"{name}\",\"path\":\"{path}\",\"recorded\":\"{recorded}\",\"source_fingerprint\":\"{fingerprint}\"}}"
		),
	)
	.unwrap();
	control
}

fn write_v3_recovery_intent(
	fixture: &Fixture,
	name: &str,
	path: &str,
	recorded: &str,
	resolved_source: &str,
) -> PathBuf {
	let control = git_path(&fixture.consumer, "gitana-submodule-update");
	std::fs::create_dir_all(&control).unwrap();
	let fingerprint = Sha256::digest(gitana_remote::redact_password(resolved_source).as_bytes());
	let fingerprint = fingerprint
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>();
	std::fs::write(
		control.join("intent.json"),
		format!(
			"{{\"version\":3,\"name\":\"{name}\",\"path\":\"{path}\",\"recorded\":\"{recorded}\",\"source_fingerprint\":\"{fingerprint}\"}}"
		),
	)
	.unwrap();
	control
}

fn write_v4_recovery_intent(
	fixture: &Fixture,
	name: &str,
	path: &str,
	recorded: &str,
	resolved_source: &str,
	source_context: &str,
) -> PathBuf {
	let control = git_path(&fixture.consumer, "gitana-submodule-update");
	std::fs::create_dir_all(&control).unwrap();
	let fingerprint = Sha256::digest(gitana_remote::redact_password(resolved_source).as_bytes());
	let fingerprint = fingerprint
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>();
	std::fs::write(
		control.join("intent.json"),
		format!(
			"{{\"version\":4,\"name\":\"{name}\",\"path\":\"{path}\",\"recorded\":\"{recorded}\",\"source_fingerprint\":\"{fingerprint}\",\"source_context\":\"{source_context}\"}}"
		),
	)
	.unwrap();
	control
}

fn write_v1_recovery_intent(
	fixture: &Fixture,
	name: &str,
	path: &str,
	recorded: &str,
	source_url: &str,
) -> PathBuf {
	let control = git_path(&fixture.consumer, "gitana-submodule-update");
	std::fs::create_dir_all(&control).unwrap();
	let fingerprint = Sha256::digest(gitana_remote::anonymize_url(source_url).as_bytes());
	let fingerprint = fingerprint
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>();
	std::fs::write(
		control.join("intent.json"),
		format!(
			"{{\"version\":1,\"name\":\"{name}\",\"path\":\"{path}\",\"recorded\":\"{recorded}\",\"source_fingerprint\":\"{fingerprint}\"}}"
		),
	)
	.unwrap();
	control
}

fn assert_recovery_precedes_normal_update(fixture: &Fixture, update: &Output, control: &Path) {
	let output = stdout(update);
	let recovered = output.find("modules/two").expect("later recovery outcome");
	let ordinary = output
		.find("modules/one")
		.expect("earlier ordinary outcome");
	assert!(
		recovered < ordinary,
		"outcomes must follow actual recovery-first completion order: {output}"
	);
	for path in ["modules/one/file.txt", "modules/two/file.txt"] {
		assert_eq!(
			std::fs::read_to_string(fixture.consumer.join(path)).unwrap(),
			"old\n"
		);
	}
	assert!(
		!control.exists(),
		"recovery intent must clear after both publications complete"
	);
}

fn assert_mount_points_at_per_worktree_repository(repository: &Path) {
	let module_git_dir = git_path(repository, "modules/one");
	let marker = std::fs::read_to_string(repository.join("modules/one/.git")).unwrap();
	let target = marker.strip_prefix("gitdir:").unwrap().trim();
	let target = repository.join("modules/one").join(target);
	assert_eq!(
		std::fs::canonicalize(target).unwrap(),
		std::fs::canonicalize(module_git_dir).unwrap()
	);
}

fn git_path(repository: &Path, path: &str) -> PathBuf {
	let rendered = git(repository, &["rev-parse", "--git-path", path]);
	let rendered = PathBuf::from(rendered.trim());
	if rendered.is_absolute() {
		rendered
	} else {
		repository.join(rendered)
	}
}

fn gta(repository: &Path, allow_file: bool, args: &[&str]) -> Output {
	if allow_file {
		gta_with_config(repository, "protocol.file.allow=always", args)
	} else {
		assert_cmd::Command::cargo_bin("gta")
			.unwrap()
			.args(["-C", repository.to_str().unwrap()])
			.args(args)
			.output()
			.expect("run gta")
	}
}

fn gta_with_config(repository: &Path, config: &str, args: &[&str]) -> Output {
	gta_with_configs(repository, &[config], args)
}

fn gta_with_configs(repository: &Path, configs: &[&str], args: &[&str]) -> Output {
	let mut command = assert_cmd::Command::cargo_bin("gta").unwrap();
	command.args(["-C", repository.to_str().unwrap()]);
	for config in configs {
		command.args(["-c", config]);
	}
	command.args(args).output().expect("run gta")
}

fn gta_with_environment(repository: &Path, args: &[&str], environment: &[(&str, &str)]) -> Output {
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", repository.to_str().unwrap()])
		.args(args)
		.envs(environment.iter().copied())
		.output()
		.expect("run gta with environment")
}

fn git_allow(repository: &Path, args: &[&str]) -> String {
	let mut full = vec!["-c", "protocol.file.allow=always"];
	full.extend_from_slice(args);
	git(repository, &full)
}

fn git_ok(repository: &Path, args: &[&str]) -> String {
	git(repository, args)
}

fn git(repository: &Path, args: &[&str]) -> String {
	let output = Command::new("git")
		.arg("-C")
		.arg(repository)
		.args(args)
		.output()
		.expect("run git");
	assert!(
		output.status.success(),
		"git {args:?} in {} failed: {}",
		repository.display(),
		String::from_utf8_lossy(&output.stderr)
	);
	String::from_utf8(output.stdout).expect("git stdout utf8")
}

fn commit(repository: &Path, message: &str) {
	let output = Command::new("git")
		.arg("-C")
		.arg(repository)
		.args([
			"-c",
			"user.name=Test",
			"-c",
			"user.email=test@example.com",
			"commit",
			"-q",
			"-m",
			message,
		])
		.output()
		.expect("commit");
	assert!(
		output.status.success(),
		"commit in {} failed: {}",
		repository.display(),
		String::from_utf8_lossy(&output.stderr)
	);
}

fn init_repository(repository: &Path, object_format: Option<&str>) {
	let mut arguments = vec!["init", "-q", "-b", "main"];
	let format;
	if let Some(value) = object_format {
		format = format!("--object-format={value}");
		arguments.push(&format);
	}
	arguments.push(".");
	git_ok(repository, &arguments);
}

fn assert_success(output: &Output, operation: &str) {
	assert!(
		output.status.success(),
		"{operation} failed:\nstdout: {}\nstderr: {}",
		stdout(output),
		stderr(output)
	);
}

fn stdout(output: &Output) -> String {
	String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
	String::from_utf8_lossy(&output.stderr).into_owned()
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQUENCE: AtomicU64 = AtomicU64::new(0);
	let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
	let path = std::env::temp_dir().join(format!(
		"gitana-submodule-command-{tag}-{}-{sequence}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&path);
	std::fs::create_dir_all(&path).unwrap();
	path
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("sha256-probe");
		let supported = Command::new("git")
			.args(["init", "--object-format=sha256"])
			.arg(&probe)
			.output()
			.is_ok_and(|output| output.status.success());
		let _ = std::fs::remove_dir_all(probe);
		supported
	})
}
