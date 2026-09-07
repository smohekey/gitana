//! End-to-end one-level submodule consumer operations against repositories created by Git.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(any(unix, windows))]
use cap_std::{ambient_authority, fs::Dir};
#[cfg(any(unix, windows))]
use gitana_submodule::acquire_submodule_config_mutation_lease;
use sha2::{Digest, Sha256};

#[test]
fn shallow_recursive_clone_and_update_bound_each_recorded_commit() {
	let root = unique_tmp("shallow-recursive-submodules");
	let leaf = root.join("leaf");
	let middle = root.join("middle");
	let superproject = root.join("super");
	for repository in [&leaf, &middle, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(repository.join("file.txt"), b"old\n").unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "old");
	}
	let leaf_recorded = git(&leaf, &["rev-parse", "HEAD"]).trim().to_owned();
	git_allow(&middle, &["submodule", "add", "../leaf", "child"]);
	git_ok(
		&middle,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.child.url",
			&format!("file://{}", leaf.display()),
		],
	);
	git_ok(&middle, &["add", ".gitmodules"]);
	commit(&middle, "add child");
	let middle_recorded = git(&middle, &["rev-parse", "HEAD"]).trim().to_owned();
	std::fs::write(leaf.join("file.txt"), b"new\n").unwrap();
	git_ok(&leaf, &["add", "file.txt"]);
	commit(&leaf, "advance leaf branch");

	git_allow(
		&superproject,
		&["submodule", "add", "../middle", "modules/middle"],
	);
	git_ok(
		&superproject,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.modules/middle.url",
			&format!("file://{}", middle.display()),
		],
	);
	git_ok(&superproject, &["add", ".gitmodules"]);
	commit(&superproject, "add middle");
	std::fs::write(middle.join("file.txt"), b"new\n").unwrap();
	git_ok(&middle, &["add", "file.txt"]);
	commit(&middle, "advance middle branch");

	let cloned = root.join("cloned");
	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules",
			"--shallow-submodules",
			superproject.to_str().unwrap(),
			cloned.to_str().unwrap(),
		],
	);
	assert_success(&clone, "shallow recursive clone");
	let cloned_middle = cloned.join("modules/middle");
	let cloned_leaf = cloned_middle.join("child");
	assert_eq!(
		git(&cloned_middle, &["rev-parse", "HEAD"]).trim(),
		middle_recorded
	);
	assert_eq!(
		git(&cloned_leaf, &["rev-parse", "HEAD"]).trim(),
		leaf_recorded
	);
	assert_shallow_one(&cloned_middle);
	assert_shallow_one(&cloned_leaf);

	let updated = root.join("updated");
	assert_success(
		&gta(
			&root,
			false,
			&[
				"clone",
				superproject.to_str().unwrap(),
				updated.to_str().unwrap(),
			],
		),
		"clone root before recursive shallow update",
	);
	assert_success(
		&gta(
			&updated,
			true,
			&[
				"submodule",
				"update",
				"--init",
				"--recursive",
				"--depth",
				"1",
			],
		),
		"recursive update at depth one",
	);
	assert_shallow_one(&updated.join("modules/middle"));
	assert_shallow_one(&updated.join("modules/middle/child"));

	let ordinary = root.join("ordinary");
	assert_success(
		&gta(
			&root,
			true,
			&[
				"clone",
				"--depth",
				"1",
				"--recurse-submodules",
				&format!("file://{}", superproject.display()),
				ordinary.to_str().unwrap(),
			],
		),
		"shallow root clone without shallow submodules",
	);
	assert_eq!(
		git(
			&ordinary.join("modules/middle"),
			&["rev-parse", "--is-shallow-repository"]
		)
		.trim(),
		"false",
		"a shallow root must not imply shallow modules"
	);

	let no_recursion = root.join("no-recursion");
	assert_success(
		&gta(
			&root,
			false,
			&[
				"clone",
				"--shallow-submodules",
				superproject.to_str().unwrap(),
				no_recursion.to_str().unwrap(),
			],
		),
		"shallow-submodules without recursion",
	);
	assert!(!no_recursion.join("modules/middle/.git").exists());

	set_module_shallow(&middle, "child", "true");
	git_ok(&middle, &["add", ".gitmodules"]);
	commit(&middle, "recommend shallow child clones");
	let middle_tip = git(&middle, &["rev-parse", "HEAD"]);
	git_allow(&superproject.join("modules/middle"), &["fetch", "origin"]);
	git_ok(
		&superproject.join("modules/middle"),
		&["checkout", "-q", middle_tip.trim()],
	);
	set_module_shallow(&superproject, "modules/middle", "true");
	git_ok(&superproject, &["add", ".gitmodules", "modules/middle"]);
	commit(&superproject, "recommend shallow recursive modules");

	let recommended = root.join("recommended");
	assert_success(
		&gta(
			&root,
			true,
			&[
				"clone",
				"--recurse-submodules",
				superproject.to_str().unwrap(),
				recommended.to_str().unwrap(),
			],
		),
		"recursive clone with per-module shallow recommendations",
	);
	assert_shallow_one(&recommended.join("modules/middle"));
	assert_shallow_one(&recommended.join("modules/middle/child"));

	let recommendations_ignored = root.join("recommendations-ignored");
	assert_success(
		&gta(
			&root,
			false,
			&[
				"clone",
				superproject.to_str().unwrap(),
				recommendations_ignored.to_str().unwrap(),
			],
		),
		"clone before ignoring recursive shallow recommendations",
	);
	assert_success(
		&gta(
			&recommendations_ignored,
			true,
			&[
				"submodule",
				"update",
				"--init",
				"--recursive",
				"--no-recommend-shallow",
			],
		),
		"recursive update ignoring per-module shallow recommendations",
	);
	assert_not_shallow(&recommendations_ignored.join("modules/middle"));
	assert_not_shallow(&recommendations_ignored.join("modules/middle/child"));
	std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clone_recurse_submodules_materializes_nested_modules_after_root_publication() {
	let root = unique_tmp("clone-recurse-submodules");
	let leaf = root.join("leaf");
	let middle = root.join("middle");
	let superproject = root.join("super");
	let consumer = root.join("consumer");
	for repository in [&leaf, &middle, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(
			repository.join("file.txt"),
			repository.display().to_string(),
		)
		.unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "root");
	}
	git_allow(&middle, &["submodule", "add", "../leaf", "child"]);
	commit(&middle, "add child");
	git_allow(
		&superproject,
		&["submodule", "add", "../middle", "modules/a"],
	);
	git_allow(&superproject, &["submodule", "add", "../leaf", "modules/b"]);
	commit(&superproject, "add modules");
	let middle_oid = git(&middle, &["rev-parse", "HEAD"]).trim().to_owned();
	let leaf_oid = git(&leaf, &["rev-parse", "HEAD"]).trim().to_owned();

	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules",
			superproject.to_str().unwrap(),
			consumer.to_str().unwrap(),
		],
	);
	assert_success(&clone, "clone --recurse-submodules");
	assert_eq!(
		git(
			&consumer,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		".\n"
	);
	let root_config = std::fs::read_to_string(git_path(&consumer, "config")).unwrap();
	assert!(
		!root_config.contains("active = true"),
		"the root-wide activation must not create redundant per-module keys"
	);
	let output = stdout(&clone);
	let cloned = output.find("Cloned '").expect("root clone outcome");
	let middle = output
		.find("Submodule path 'modules/a'")
		.expect("middle submodule outcome");
	let sibling = output
		.find("Submodule path 'modules/b'")
		.expect("sibling submodule outcome");
	let child = output
		.find("Submodule path 'modules/a/child'")
		.expect("nested submodule outcome");
	assert!(
		cloned < middle && middle < sibling && sibling < child,
		"root publication and repository-local batches must retain their order: {output}"
	);
	assert_eq!(
		git(&consumer.join("modules/a"), &["rev-parse", "HEAD"]).trim(),
		middle_oid
	);
	assert_eq!(
		git(&consumer.join("modules/a/child"), &["rev-parse", "HEAD"]).trim(),
		leaf_oid
	);
	let middle_config =
		std::fs::read_to_string(git_path(&consumer.join("modules/a"), "config")).unwrap();
	assert!(
		middle_config.contains("active = true"),
		"descendants must retain normal per-module activation"
	);
	assert_eq!(
		stdout(&gta(
			&consumer,
			false,
			&["submodule", "status", "--recursive"]
		)),
		format!(" {middle_oid} modules/a\n {leaf_oid} modules/a/child\n {leaf_oid} modules/b\n")
	);
	std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clone_recurse_submodules_honors_root_pathspecs() {
	let root = unique_tmp("clone-recurse-submodules-pathspecs");
	let leaf = root.join("leaf");
	let middle = root.join("middle");
	let superproject = root.join("super");
	for repository in [&leaf, &middle, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(
			repository.join("file.txt"),
			repository.display().to_string(),
		)
		.unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "root");
	}
	git_allow(&middle, &["submodule", "add", "../leaf", "child"]);
	commit(&middle, "add child");
	git_allow(
		&superproject,
		&["submodule", "add", "../middle", "modules/a"],
	);
	git_allow(&superproject, &["submodule", "add", "../leaf", "modules/b"]);
	commit(&superproject, "add modules");

	let selected = root.join("selected");
	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules=modules/a",
			superproject.to_str().unwrap(),
			selected.to_str().unwrap(),
		],
	);
	assert_success(&clone, "clone with one recursive pathspec");
	assert!(selected.join("modules/a/.git").is_file());
	assert!(selected.join("modules/a/child/.git").is_file());
	assert!(!selected.join("modules/b/.git").exists());
	assert_eq!(
		git(
			&selected,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		"modules/a\n"
	);
	for (tag, selector) in [
		("selected-directory", "modules/a/"),
		("selected-directory-dot", "modules/a/."),
	] {
		let directory_selected = root.join(tag);
		let clone = gta(
			&root,
			true,
			&[
				"clone",
				&format!("--recurse-submodules={selector}"),
				superproject.to_str().unwrap(),
				directory_selected.to_str().unwrap(),
			],
		);
		assert_success(&clone, "clone with a directory-form pathspec");
		assert!(directory_selected.join("modules/a/.git").is_file());
		assert!(directory_selected.join("modules/a/child/.git").is_file());
		assert!(!directory_selected.join("modules/b/.git").exists());
		assert_eq!(
			git(
				&directory_selected,
				&["config", "--local", "--get-all", "submodule.active"]
			),
			format!("{selector}\n")
		);
	}

	let layered = root.join("layered");
	let clone = gta_with_configs(
		&root,
		&["protocol.file.allow=always", "submodule.active=modules/b"],
		&[
			"clone",
			"--recurse-submodules=modules/a",
			superproject.to_str().unwrap(),
			layered.to_str().unwrap(),
		],
	);
	assert_success(&clone, "clone with layered recursive pathspecs");
	assert!(layered.join("modules/a/.git").is_file());
	assert!(layered.join("modules/b/.git").is_file());
	assert_eq!(
		git(
			&layered,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		"modules/a\n",
		"only the command-line selector is persisted locally"
	);

	for (tag, selector) in [
		("wildcard-directory", "modules/*/"),
		("wildcard-directory-magic", ":(glob)modules/*/"),
	] {
		let wildcard = root.join(tag);
		let clone = gta(
			&root,
			true,
			&[
				"clone",
				&format!("--recurse-submodules={selector}"),
				superproject.to_str().unwrap(),
				wildcard.to_str().unwrap(),
			],
		);
		assert_success(&clone, "clone with a wildcard directory pathspec");
		assert!(!wildcard.join("modules/a/.git").exists());
		assert!(!wildcard.join("modules/b/.git").exists());
	}

	let wildcard = root.join("wildcard");
	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules=modules/*",
			superproject.to_str().unwrap(),
			wildcard.to_str().unwrap(),
		],
	);
	assert_success(&clone, "clone with a wildcard pathspec");
	assert!(wildcard.join("modules/a/.git").is_file());
	assert!(wildcard.join("modules/a/child/.git").is_file());
	assert!(wildcard.join("modules/b/.git").is_file());

	let wildcard_excluded = root.join("wildcard-excluded");
	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules=:(exclude,glob)modules/*/",
			superproject.to_str().unwrap(),
			wildcard_excluded.to_str().unwrap(),
		],
	);
	assert_success(&clone, "clone with a wildcard directory exclusion");
	assert!(wildcard_excluded.join("modules/a/.git").is_file());
	assert!(wildcard_excluded.join("modules/a/child/.git").is_file());
	assert!(wildcard_excluded.join("modules/b/.git").is_file());

	let repeated = root.join("repeated");
	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules=modules/a",
			"--recursive=modules/b",
			superproject.to_str().unwrap(),
			repeated.to_str().unwrap(),
		],
	);
	assert_success(&clone, "clone with repeated recursive pathspecs");
	assert!(repeated.join("modules/a/.git").is_file());
	assert!(repeated.join("modules/a/child/.git").is_file());
	assert!(repeated.join("modules/b/.git").is_file());
	assert_eq!(
		git(
			&repeated,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		"modules/a\nmodules/b\n"
	);

	let excluded = root.join("excluded");
	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules=:(exclude)modules/b/",
			superproject.to_str().unwrap(),
			excluded.to_str().unwrap(),
		],
	);
	assert_success(&clone, "clone with an exclusion-only recursive pathspec");
	assert!(excluded.join("modules/a/.git").is_file());
	assert!(excluded.join("modules/a/child/.git").is_file());
	assert!(!excluded.join("modules/b/.git").exists());
	assert_eq!(
		git(
			&excluded,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		":(exclude)modules/b/\n"
	);

	let unmatched = root.join("unmatched");
	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules=missing",
			superproject.to_str().unwrap(),
			unmatched.to_str().unwrap(),
		],
	);
	assert_success(&clone, "clone with an unmatched recursive pathspec");
	assert!(!unmatched.join("modules/a/.git").exists());
	assert!(!unmatched.join("modules/b/.git").exists());
	assert_eq!(
		git(
			&unmatched,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		"missing\n"
	);
	let strict = gta(&unmatched, true, &["submodule", "update", "missing"]);
	assert!(!strict.status.success());
	assert!(
		stderr(&strict).contains("pathspec 'missing' did not match any file known to git"),
		"ordinary update pathspecs must remain strict: {}",
		stderr(&strict)
	);

	let invalid = root.join("invalid");
	let clone = gta(
		&root,
		true,
		&[
			"clone",
			"--recurse-submodules=",
			superproject.to_str().unwrap(),
			invalid.to_str().unwrap(),
		],
	);
	assert!(
		!clone.status.success(),
		"an empty pathspec must be rejected"
	);
	assert!(
		invalid.join(".git").is_dir(),
		"pathspec validation happens after root publication"
	);
	assert_eq!(
		git(
			&invalid,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		"\n"
	);

	std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_recurse_submodules_carries_url_credentials_through_relative_children() {
	if !support::git_http_backend_available() {
		eprintln!("skipping: git http-backend not available");
		return;
	}
	let root = unique_tmp("clone-recurse-submodules-auth");
	let leaf = root.join("leaf");
	let middle = root.join("rewritten-middle");
	let superproject = root.join("super");
	let consumer = root.join("consumer");
	for repository in [&leaf, &middle, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(
			repository.join("file.txt"),
			repository.display().to_string(),
		)
		.unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "root");
	}
	git_allow(&middle, &["submodule", "add", "../leaf", "child"]);
	commit(&middle, "add child");
	git_allow(
		&superproject,
		&["submodule", "add", "../rewritten-middle", "parent"],
	);
	git_ok(
		&superproject,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.parent.url",
			"../middle",
		],
	);
	commit(&superproject, "add parent");
	let middle_oid = git(&middle, &["rev-parse", "HEAD"]).trim().to_owned();
	let leaf_oid = git(&leaf, &["rev-parse", "HEAD"]).trim().to_owned();
	let base = support::serve_git_http_backend_basic_auth(root.clone(), "alice", "s3cr3t").await;
	let authenticated = format!(
		"http://alice:s3cr3t@{}/super",
		base.trim_start_matches("http://")
	);
	let safe_middle = format!("http://alice@{}/middle", base.trim_start_matches("http://"));
	let rewritten_middle = format!(
		"http://alice@{}/rewritten-middle",
		base.trim_start_matches("http://")
	);
	let global = root.join("clone.config");
	std::fs::write(
		&global,
		format!("[url \"{rewritten_middle}\"]\n\tinsteadOf = {safe_middle}\n"),
	)
	.unwrap();

	let clone = gta_with_environment(
		&root,
		&[
			"clone",
			"--recurse-submodules",
			&authenticated,
			consumer.to_str().unwrap(),
		],
		&[
			("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
			("GIT_CONFIG_SYSTEM", "/dev/null"),
			("GIT_TERMINAL_PROMPT", "0"),
		],
	);
	assert_success(&clone, "authenticated recursive clone");
	assert_eq!(
		git(&consumer.join("parent"), &["rev-parse", "HEAD"]).trim(),
		middle_oid
	);
	assert_eq!(
		git(&consumer.join("parent/child"), &["rev-parse", "HEAD"]).trim(),
		leaf_oid
	);
	assert_eq!(
		git(
			&consumer.join("parent"),
			&["config", "--get", "remote.origin.url"]
		)
		.trim(),
		rewritten_middle,
		"the module must persist the rewritten endpoint without its password"
	);
	for config in [
		git_path(&consumer, "config"),
		git_path(&consumer.join("parent"), "config"),
		git_path(&consumer.join("parent/child"), "config"),
	] {
		let text = std::fs::read_to_string(&config).unwrap();
		assert!(
			!text.contains("s3cr3t"),
			"the recursive credential leaked into {}: {text}",
			config.display()
		);
	}
	for reflog in [
		git_path(&consumer, "logs/HEAD"),
		git_path(&consumer.join("parent"), "logs/HEAD"),
		git_path(&consumer.join("parent/child"), "logs/HEAD"),
	] {
		let text = std::fs::read_to_string(&reflog).unwrap();
		assert!(
			!text.contains("s3cr3t"),
			"the recursive credential leaked into {}: {text}",
			reflog.display()
		);
	}
	assert!(!stdout(&clone).contains("s3cr3t"));
	assert!(!stderr(&clone).contains("s3cr3t"));
	std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clone_recurse_submodules_preserves_root_inactivity() {
	let root = unique_tmp("clone-recurse-submodules-inactive");
	let leaf = root.join("leaf");
	let middle = root.join("middle");
	let superproject = root.join("super");
	for repository in [&leaf, &middle, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(
			repository.join("file.txt"),
			repository.display().to_string(),
		)
		.unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "root");
	}
	git_allow(&middle, &["submodule", "add", "../leaf", "child"]);
	commit(&middle, "add child");
	git_allow(&superproject, &["submodule", "add", "../middle", "parent"]);
	git_allow(&superproject, &["submodule", "add", "../leaf", "active"]);
	commit(&superproject, "add modules");

	for (name, inactive) in [
		("named", "submodule.parent.active=false"),
		("pathspec", "submodule.active=:(exclude)parent"),
	] {
		let consumer = root.join(format!("consumer-{name}"));
		let clone = gta_with_configs(
			&root,
			&["protocol.file.allow=always", inactive],
			&[
				"clone",
				"--recurse-submodules",
				superproject.to_str().unwrap(),
				consumer.to_str().unwrap(),
			],
		);
		assert_success(&clone, "active-aware recursive clone");
		assert!(consumer.join("active/.git").is_file());
		assert!(!consumer.join("parent/.git").exists());
		assert!(!consumer.join("parent/child/.git").exists());
		let config = std::fs::read_to_string(git_path(&consumer, "config")).unwrap();
		assert!(config.contains("active = ."));
		assert!(config.contains("[submodule \"active\"]"));
		assert!(
			!config.contains("[submodule \"parent\"]"),
			"inactive root module must not be registered: {config}"
		);
		assert!(
			!config.contains("active = true"),
			"root-wide activation must not be replaced by per-module activation: {config}"
		);
	}

	let global = root.join("inactive.config");
	std::fs::write(
		&global,
		"[protocol \"file\"]\n\tallow = always\n[submodule \"parent\"]\n\tactive = false\n\turl\n",
	)
	.unwrap();
	let consumer = root.join("consumer-valueless-url");
	let clone = gta_with_environment(
		&root,
		&[
			"clone",
			"--recurse-submodules",
			superproject.to_str().unwrap(),
			consumer.to_str().unwrap(),
		],
		&[
			("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
			("GIT_CONFIG_SYSTEM", "/dev/null"),
		],
	);
	assert_success(&clone, "inactive module with a valueless URL");
	assert!(consumer.join("active/.git").is_file());
	assert!(!consumer.join("parent/.git").exists());
	assert!(!consumer.join("parent/child/.git").exists());
	let config = std::fs::read_to_string(git_path(&consumer, "config")).unwrap();
	assert!(config.contains("active = ."));
	assert!(config.contains("[submodule \"active\"]"));
	assert!(!config.contains("[submodule \"parent\"]"));
	std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clone_recursive_alias_is_opt_in() {
	let fixture = Fixture::new("clone-recursive-alias");
	let recursive = fixture.root.join("recursive");
	let plain = fixture.root.join("plain");
	let no_modules = fixture.root.join("no-modules");
	std::fs::create_dir(&recursive).unwrap();
	let recursive_clone = gta(
		&fixture.root,
		true,
		&[
			"clone",
			"--recursive",
			fixture.superproject.to_str().unwrap(),
			recursive.to_str().unwrap(),
		],
	);
	assert_success(&recursive_clone, "clone --recursive");
	assert_eq!(
		std::fs::read_to_string(recursive.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);

	let plain_clone = gta(
		&fixture.root,
		true,
		&[
			"clone",
			fixture.superproject.to_str().unwrap(),
			plain.to_str().unwrap(),
		],
	);
	assert_success(&plain_clone, "plain clone");
	assert!(
		!plain.join("modules/one/.git").exists(),
		"clone recursion must remain opt in"
	);
	let plain_config = std::fs::read_to_string(git_path(&plain, "config")).unwrap();
	assert!(!plain_config.contains("active = ."));

	let no_modules_clone = gta(
		&fixture.root,
		true,
		&[
			"clone",
			"--recurse-submodules",
			fixture.source.to_str().unwrap(),
			no_modules.to_str().unwrap(),
		],
	);
	assert_success(
		&no_modules_clone,
		"recursive clone without submodule declarations",
	);
	assert_eq!(
		std::fs::read_to_string(no_modules.join("file.txt")).unwrap(),
		"old\n"
	);
	assert_eq!(
		git(
			&no_modules,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		".\n"
	);
}

#[test]
fn clone_recursion_failure_retains_the_root_for_retry() {
	let fixture = Fixture::new("clone-recursion-retry");
	fixture.commit_source("new\n", "advance source past recorded gitlink");
	git_allow(
		&fixture.superproject,
		&[
			"submodule",
			"add",
			"--name",
			"two",
			"../source",
			"modules/two",
		],
	);
	commit(&fixture.superproject, "add unselected submodule");
	let file_url = format!("file://{}", fixture.source.display());
	for name in ["one", "two"] {
		git_ok(
			&fixture.superproject,
			&[
				"config",
				"-f",
				".gitmodules",
				&format!("submodule.{name}.url"),
				&file_url,
			],
		);
	}
	git_ok(&fixture.superproject, &["add", ".gitmodules"]);
	commit(&fixture.superproject, "use file URLs for shallow retry");
	let consumer = fixture.root.join("retry-consumer");
	let clone = gta(
		&fixture.root,
		false,
		&[
			"clone",
			"--recurse-submodules=modules/one",
			"--shallow-submodules",
			fixture.superproject.to_str().unwrap(),
			consumer.to_str().unwrap(),
		],
	);
	assert!(
		!clone.status.success(),
		"recursive file transport must fail closed"
	);
	assert!(
		stdout(&clone).starts_with("Cloned '"),
		"root publication must be reported before recursion: {}",
		stdout(&clone)
	);
	assert!(
		consumer.join(".git").is_dir(),
		"the root clone must be retained"
	);
	assert_eq!(
		git(&consumer, &["rev-parse", "HEAD"]),
		git(&fixture.superproject, &["rev-parse", "HEAD"])
	);
	assert_eq!(
		git(
			&consumer,
			&["config", "--local", "--get-all", "submodule.active"]
		),
		"modules/one\n"
	);

	let retry = gta(
		&consumer,
		true,
		&[
			"submodule",
			"update",
			"--init",
			"--recursive",
			"--depth",
			"1",
		],
	);
	assert_success(&retry, "retry retained clone submodules");
	assert_eq!(
		std::fs::read_to_string(consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
	assert_shallow_one(&consumer.join("modules/one"));
	assert!(
		!consumer.join("modules/two/.git").exists(),
		"retry must not materialize a module outside the persisted clone selector"
	);
	assert!(
		!std::fs::read_to_string(git_path(&consumer, "config"))
			.unwrap()
			.contains("[submodule \"two\"]"),
		"retry must not register a module outside the persisted clone selector"
	);
}

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
fn submodule_update_depth_honors_file_urls_and_updates_existing_repositories() {
	let shallow = Fixture::new("submodule-update-file-depth");
	let advertised = shallow.commit_source("new\n", "advance source past recorded gitlink");
	git_ok(
		&shallow.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			&format!("file://{}", shallow.source.display()),
		],
	);
	let update = gta(
		&shallow.consumer,
		true,
		&["submodule", "update", "--init", "--depth", "1"],
	);
	assert_success(&update, "new file-URL module at depth one");
	let module = shallow.consumer.join("modules/one");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), shallow.old);
	assert_shallow_one(&module);
	assert_eq!(
		git(&module, &["cat-file", "-t", &advertised]).trim(),
		"commit",
		"the advertised tip fetched separately from the recorded commit must remain available"
	);
	assert_eq!(
		git(&module, &["rev-list", "--count", &advertised]).trim(),
		"1",
		"the advertised tip must retain its shallow boundary"
	);

	let local = Fixture::new("submodule-update-local-depth");
	local.commit_source("new\n", "advance raw local source");
	assert_success(
		&gta(
			&local.consumer,
			true,
			&["submodule", "update", "--init", "--depth", "1"],
		),
		"raw local module with ignored depth",
	);
	assert_eq!(
		git(
			&local.consumer.join("modules/one"),
			&["rev-parse", "--is-shallow-repository"]
		)
		.trim(),
		"false"
	);
	let local_recorded = local.commit_source("later\n", "new recorded raw local commit");
	local.record_superproject_commit(&local_recorded, "advance raw local gitlink");
	git_ok(&local.consumer, &["fetch", "origin"]);
	git_ok(&local.consumer, &["reset", "--hard", "origin/main"]);
	assert_success(
		&gta(
			&local.consumer,
			true,
			&["submodule", "update", "--depth", "1"],
		),
		"existing raw local module at depth one",
	);
	let local_module = local.consumer.join("modules/one");
	assert_eq!(
		git(&local_module, &["rev-parse", "HEAD"]).trim(),
		local_recorded
	);
	assert_shallow_one(&local_module);

	let existing = Fixture::new("submodule-update-existing-depth");
	let file_url = format!("file://{}", existing.source.display());
	git_ok(
		&existing.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			&file_url,
		],
	);
	assert_success(
		&gta(&existing.consumer, true, &["submodule", "update", "--init"]),
		"initial full module update",
	);
	let new = existing.commit_source("new\n", "new recorded source commit");
	existing.record_superproject_commit(&new, "advance gitlink");
	git_ok(&existing.consumer, &["fetch", "origin"]);
	git_ok(&existing.consumer, &["reset", "--hard", "origin/main"]);
	assert_success(
		&gta(
			&existing.consumer,
			true,
			&["submodule", "update", "--depth", "1"],
		),
		"existing module update at depth one",
	);
	let existing_module = existing.consumer.join("modules/one");
	assert_eq!(git(&existing_module, &["rev-parse", "HEAD"]).trim(), new);
	assert_shallow_one(&existing_module);
}

#[test]
fn recommended_shallow_only_applies_when_creating_module_repositories() {
	let ignored = Fixture::new("submodule-recommended-shallow-ignored");
	ignored.commit_source("new\n", "advance source before full module clone");
	configure_module_shallow(&ignored.consumer, "one", &ignored.source, "true");
	assert_success(
		&gta(
			&ignored.consumer,
			true,
			&[
				"submodule",
				"update",
				"--init",
				"--recommend-shallow",
				"--no-recommend-shallow",
			],
		),
		"last no-recommend-shallow disables the recommendation",
	);
	let ignored_module = ignored.consumer.join("modules/one");
	assert_not_shallow(&ignored_module);

	let recorded = ignored.commit_source("later\n", "advance existing module source");
	ignored.record_superproject_commit(&recorded, "advance existing module gitlink");
	git_ok(&ignored.consumer, &["fetch", "origin"]);
	git_ok(&ignored.consumer, &["reset", "--hard", "origin/main"]);
	configure_module_shallow(&ignored.consumer, "one", &ignored.source, "true");
	assert_success(
		&gta(&ignored.consumer, true, &["submodule", "update"]),
		"a recommendation does not truncate an existing repository",
	);
	assert_eq!(
		git(&ignored_module, &["rev-parse", "HEAD"]).trim(),
		recorded
	);
	assert_not_shallow(&ignored_module);

	let recommended = Fixture::new("submodule-recommended-shallow-enabled");
	recommended.commit_source("new\n", "advance recommended source");
	configure_module_shallow(&recommended.consumer, "one", &recommended.source, "true");
	assert_success(
		&gta(
			&recommended.consumer,
			true,
			&[
				"submodule",
				"update",
				"--init",
				"--no-recommend-shallow",
				"--recommend-shallow",
			],
		),
		"last recommend-shallow enables the recommendation",
	);
	assert_shallow_one(&recommended.consumer.join("modules/one"));

	let explicit = Fixture::new("submodule-recommended-shallow-explicit-depth");
	explicit.commit_source("new\n", "advance explicitly shallow source");
	configure_module_shallow(&explicit.consumer, "one", &explicit.source, "false");
	assert_success(
		&gta(
			&explicit.consumer,
			true,
			&[
				"submodule",
				"update",
				"--init",
				"--no-recommend-shallow",
				"--depth",
				"1",
			],
		),
		"explicit depth overrides disabled and false recommendations",
	);
	assert_shallow_one(&explicit.consumer.join("modules/one"));
}

#[test]
fn clone_shallow_submodule_negation_preserves_per_module_recommendations() {
	let fixture = Fixture::new("clone-shallow-submodule-negation");
	fixture.commit_source("new\n", "advance clone recommendation source");
	configure_module_shallow(&fixture.superproject, "one", &fixture.source, "false");
	git_ok(&fixture.superproject, &["add", ".gitmodules"]);
	commit(
		&fixture.superproject,
		"disable module shallow recommendation",
	);

	let disabled = fixture.root.join("clone-shallow-disabled");
	assert_success(
		&gta(
			&fixture.root,
			true,
			&[
				"clone",
				"--recurse-submodules",
				"--shallow-submodules",
				"--no-shallow-submodules",
				fixture.superproject.to_str().unwrap(),
				disabled.to_str().unwrap(),
			],
		),
		"last no-shallow-submodules cancels the global shallow force",
	);
	assert_not_shallow(&disabled.join("modules/one"));

	let enabled = fixture.root.join("clone-shallow-enabled");
	assert_success(
		&gta(
			&fixture.root,
			true,
			&[
				"clone",
				"--recurse-submodules",
				"--no-shallow-submodules",
				"--shallow-submodules",
				fixture.superproject.to_str().unwrap(),
				enabled.to_str().unwrap(),
			],
		),
		"last shallow-submodules enables the global shallow force",
	);
	assert_shallow_one(&enabled.join("modules/one"));

	set_module_shallow(&fixture.superproject, "one", "true");
	git_ok(&fixture.superproject, &["add", ".gitmodules"]);
	commit(&fixture.superproject, "recommend shallow module clones");
	let recommended = fixture.root.join("clone-shallow-recommended");
	assert_success(
		&gta(
			&fixture.root,
			true,
			&[
				"clone",
				"--recurse-submodules",
				"--no-shallow-submodules",
				fixture.superproject.to_str().unwrap(),
				recommended.to_str().unwrap(),
			],
		),
		"no-shallow-submodules retains the per-module recommendation",
	);
	assert_shallow_one(&recommended.join("modules/one"));
}

#[test]
fn submodule_update_rejects_zero_depth_before_mutation() {
	let fixture = Fixture::new("submodule-update-zero-depth");
	let config = git_path(&fixture.consumer, "config");
	let before = std::fs::read(&config).unwrap();
	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "--depth", "0"],
	);
	assert!(!update.status.success(), "zero depth must be rejected");
	assert!(
		stderr(&update).contains("--depth must be a positive number of commits"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert_eq!(std::fs::read(&config).unwrap(), before);
	assert!(!fixture.consumer.join("modules/one/.git").exists());
	assert!(!git_path(&fixture.consumer, "gitana-submodule-update").exists());
}

#[test]
fn malformed_shallow_recommendation_fails_before_initialization() {
	let fixture = Fixture::new("submodule-invalid-shallow-recommendation");
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"--add",
			"submodule.unselected.shallow",
			"invalid",
		],
	);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"--add",
			"submodule.unselected.shallow",
			"false",
		],
	);
	let config = git_path(&fixture.consumer, "config");
	let before = std::fs::read(&config).unwrap();
	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(!update.status.success(), "malformed boolean must fail");
	assert!(
		stderr(&update).contains("not a boolean"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert_eq!(std::fs::read(config).unwrap(), before);
	assert!(!fixture.consumer.join("modules/one/.git").exists());
	assert!(!git_path(&fixture.consumer, "modules/one").exists());
}

#[test]
fn implicit_init_honors_root_activation_while_explicit_paths_override_it() {
	let fixture = Fixture::new("init-active-pathspec");
	add_second_module_mapping(&fixture);
	git_ok(
		&fixture.consumer,
		&["config", "submodule.active", "modules/one"],
	);

	let init = gta(&fixture.consumer, true, &["submodule", "init"]);
	assert_success(&init, "implicit init with a root activation pathspec");
	let config = std::fs::read_to_string(git_path(&fixture.consumer, "config")).unwrap();
	assert!(config.contains("[submodule \"one\"]"));
	assert!(!config.contains("[submodule \"two\"]"));

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "implicit update with a root activation pathspec");
	assert!(fixture.consumer.join("modules/one/.git").is_file());
	assert!(!fixture.consumer.join("modules/two/.git").exists());

	let explicit = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "modules/two"],
	);
	assert_success(&explicit, "explicit update outside the active pathspec");
	assert!(fixture.consumer.join("modules/two/.git").is_file());
}

#[test]
fn recursive_status_and_update_traverse_nested_modules_with_explicit_level_order() {
	let root = unique_tmp("recursive-status-update");
	let leaf = root.join("leaf");
	let middle = root.join("middle");
	let superproject = root.join("super");
	let consumer = root.join("consumer");
	for repository in [&leaf, &middle, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(
			repository.join("file.txt"),
			repository.display().to_string(),
		)
		.unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "root");
	}
	git_allow(&middle, &["submodule", "add", "../leaf", "child"]);
	commit(&middle, "add child");
	git_allow(
		&superproject,
		&["submodule", "add", "../middle", "modules/a"],
	);
	git_allow(&superproject, &["submodule", "add", "../leaf", "modules/b"]);
	commit(&superproject, "add modules");
	let clone = Command::new("git")
		.args(["clone", "-q", "--no-recurse-submodules"])
		.arg(&superproject)
		.arg(&consumer)
		.output()
		.unwrap();
	assert_success(&clone, "clone recursive fixture");
	let middle_oid = git(&middle, &["rev-parse", "HEAD"]).trim().to_owned();
	let leaf_oid = git(&leaf, &["rev-parse", "HEAD"]).trim().to_owned();

	let initial = gta(&consumer, false, &["submodule", "status", "--recursive"]);
	assert_success(&initial, "recursive status before initialization");
	assert_eq!(
		stdout(&initial),
		format!("-{middle_oid} modules/a\n-{leaf_oid} modules/b\n")
	);

	let update = gta(
		&consumer,
		true,
		&["submodule", "update", "--init", "--recursive"],
	);
	assert_success(&update, "recursive update --init");
	assert_eq!(
		stdout(&update),
		format!(
			"Submodule path 'modules/a': checked out '{middle_oid}'\nSubmodule path 'modules/b': checked out '{leaf_oid}'\nSubmodule path 'modules/a/child': checked out '{leaf_oid}'\n"
		),
		"the whole root batch must complete before entering its child"
	);

	let status = gta(&consumer, false, &["submodule", "status", "--recursive"]);
	assert_success(&status, "recursive status after update");
	assert_eq!(
		stdout(&status),
		format!(" {middle_oid} modules/a\n {leaf_oid} modules/a/child\n {leaf_oid} modules/b\n"),
		"status must render depth-first"
	);
	let module = consumer.join("modules/a");
	let intermediate = consumer.join("modules/a-case-rename");
	let recased = consumer.join("modules/A");
	std::fs::rename(&module, &intermediate).unwrap();
	std::fs::rename(&intermediate, &recased).unwrap();
	if module.exists() {
		let status = gta(
			&consumer,
			false,
			&["submodule", "status", "--recursive", "modules/a"],
		);
		assert_success(
			&status,
			"recursive status with a native-equivalent mount spelling",
		);
		let update = gta(
			&consumer,
			true,
			&["submodule", "update", "--recursive", "modules/a"],
		);
		assert_success(
			&update,
			"recursive update with a native-equivalent mount spelling",
		);
	}
	std::fs::rename(&recased, &intermediate).unwrap();
	std::fs::rename(&intermediate, &module).unwrap();
	let child_source = git(
		&consumer.join("modules/a/child"),
		&["config", "--get", "remote.origin.url"],
	)
	.trim()
	.to_owned();
	let unrelated_recovery = write_v4_recovery_intent_at(
		&consumer.join("modules/a"),
		"child",
		"child",
		&leaf_oid,
		&child_source,
		"module",
	);
	let empty_owner = write_v4_recovery_intent_at(
		&consumer,
		"empty",
		"",
		&middle_oid,
		"https://example.invalid/empty",
		"module",
	);
	let refused = gta(
		&consumer,
		true,
		&["submodule", "update", "--recursive", "modules/b"],
	);
	assert!(!refused.status.success());
	assert!(
		stderr(&refused).contains("staging intent has an empty path"),
		"unexpected error: {}",
		stderr(&refused)
	);
	assert!(
		unrelated_recovery.exists(),
		"an empty root owner must not recover an unrelated descendant journal"
	);
	assert!(
		empty_owner.exists(),
		"the malformed root journal must remain available for diagnosis"
	);
	std::fs::remove_dir_all(empty_owner).unwrap();
	std::fs::remove_dir_all(unrelated_recovery).unwrap();

	let nested_update = git_path(&consumer.join("modules/a"), "gitana-submodule-update");
	std::fs::create_dir_all(&nested_update).unwrap();
	let parent_update = gta(&consumer, true, &["submodule", "update", "modules/a"]);
	assert!(!parent_update.status.success());
	assert!(
		stderr(&parent_update).contains("pending nested submodule update recovery"),
		"unexpected error: {}",
		stderr(&parent_update)
	);
	let recovery = gta(&consumer, true, &["submodule", "update", "--recursive"]);
	assert_success(&recovery, "deepest-first nested update recovery");
	assert!(!nested_update.exists());

	let nested_deinit = git_path(&consumer.join("modules/a"), "gitana-submodule-deinit");
	std::fs::create_dir_all(&nested_deinit).unwrap();
	let parent_update = gta(&consumer, true, &["submodule", "update", "modules/a"]);
	assert!(!parent_update.status.success());
	assert!(
		stderr(&parent_update).contains("pending nested submodule deinit recovery"),
		"unexpected error: {}",
		stderr(&parent_update)
	);
	let refused = gta(&consumer, true, &["submodule", "update", "--recursive"]);
	assert!(!refused.status.success());
	assert!(
		stderr(&refused).contains("pending submodule deinit"),
		"unexpected error: {}",
		stderr(&refused)
	);
	std::fs::remove_dir(&nested_deinit).unwrap();

	let nested_update = write_v4_recovery_intent_at(
		&consumer.join("modules/a"),
		"child",
		"child",
		&leaf_oid,
		&child_source,
		"module",
	);
	let super_config = consumer.join(".git/config");
	let super_before = std::fs::read(&super_config).unwrap();
	let module_config = git_path(&consumer.join("modules/a"), "config");
	let module_before = std::fs::read(&module_config).unwrap();
	let refused = gta(&consumer, false, &["submodule", "deinit", "modules/a"]);
	assert!(!refused.status.success());
	assert!(
		stderr(&refused).contains("pending submodule update recovery in module 'modules/a'"),
		"unexpected error: {}",
		stderr(&refused)
	);
	assert!(consumer.join("modules/a/file.txt").is_file());
	assert_eq!(std::fs::read(&super_config).unwrap(), super_before);
	assert_eq!(std::fs::read(&module_config).unwrap(), module_before);
	assert!(
		!git_path(&consumer, "gitana-submodule-deinit").exists(),
		"parent deinit must fail before publishing an intent"
	);

	let parent_source = git(
		&consumer.join("modules/a"),
		&["config", "--get", "remote.origin.url"],
	)
	.trim()
	.to_owned();
	let root_update = write_v4_recovery_intent_at(
		&consumer,
		"modules/a",
		"modules/a",
		&middle_oid,
		&parent_source,
		"module",
	);
	let recovered = gta(
		&consumer,
		true,
		&["submodule", "update", "--recursive", "modules/b"],
	);
	assert_success(
		&recovered,
		"recover the unselected root owner and its child deepest-first",
	);
	assert!(
		!nested_update.exists(),
		"the child journal beneath the root owner must be recovered"
	);
	assert!(
		!root_update.exists(),
		"the root owner's journal must be recovered"
	);

	let nested_update = write_v4_recovery_intent_at(
		&consumer.join("modules/a"),
		"child",
		"child",
		&leaf_oid,
		&child_source,
		"module",
	);
	let sibling_source = git(
		&consumer.join("modules/b"),
		&["config", "--get", "remote.origin.url"],
	)
	.trim()
	.to_owned();
	let root_update = write_v4_recovery_intent_at(
		&consumer,
		"modules/b",
		"modules/b",
		&leaf_oid,
		&sibling_source,
		"module",
	);
	let recovered = gta(
		&consumer,
		true,
		&[
			"submodule",
			"update",
			"--recursive",
			":(exclude)not-present",
		],
	);
	assert_success(
		&recovered,
		"preserve exclusion-only selection while scanning a distinct recovery owner",
	);
	assert!(
		!nested_update.exists(),
		"exclusion-only recovery must retain the originally selected subtree"
	);
	assert!(
		!root_update.exists(),
		"the distinct root owner must also recover"
	);
	let deinit = gta(
		&consumer,
		false,
		&["submodule", "deinit", "--force", "modules/a"],
	);
	assert_success(&deinit, "deinit after nested update recovery");

	let no_init = root.join("consumer-no-init");
	let clone = Command::new("git")
		.args(["clone", "-q", "--no-recurse-submodules"])
		.arg(&superproject)
		.arg(&no_init)
		.output()
		.unwrap();
	assert_success(&clone, "clone no-init recursive fixture");
	let update = gta(&no_init, true, &["submodule", "update", "--recursive"]);
	assert_success(&update, "recursive update without init");
	assert_eq!(stdout(&update), "");
	assert!(!no_init.join("modules/a/.git").exists());

	let selected = root.join("consumer-selected");
	let clone = Command::new("git")
		.args(["clone", "-q", "--no-recurse-submodules"])
		.arg(&superproject)
		.arg(&selected)
		.output()
		.unwrap();
	assert_success(&clone, "clone selected recursive fixture");
	let update = gta(
		&selected.join("modules"),
		true,
		&["submodule", "update", "--init", "--recursive", "a"],
	);
	assert_success(&update, "path-selected recursive update");
	assert_eq!(
		stdout(&update),
		format!(
			"Submodule path 'a': checked out '{middle_oid}'\nSubmodule path 'a/child': checked out '{leaf_oid}'\n"
		)
	);
	assert!(!selected.join("modules/b/.git").exists());
	let initialized = gta(&selected, false, &["submodule", "init", "modules/b"]);
	assert_success(&initialized, "register the unselected sibling");
	let empty_control = git_path(&selected, "gitana-submodule-update");
	std::fs::create_dir_all(&empty_control).unwrap();
	let recovered = gta(
		&selected,
		true,
		&["submodule", "update", "--init", "--recursive", "modules/a"],
	);
	assert_success(&recovered, "retire an empty update control");
	assert!(
		!selected.join("modules/b/.git").exists(),
		"empty-control recovery must not broaden the requested root selection"
	);
	assert!(
		!empty_control.exists(),
		"empty-control recovery must retire the orphaned namespace"
	);
	std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recursive_update_rejects_a_child_common_directory_redirect_before_config_mutation() {
	let root = unique_tmp("recursive-child-commondir");
	let leaf = root.join("leaf");
	let middle = root.join("middle");
	let superproject = root.join("super");
	let consumer = root.join("consumer");
	let foreign = root.join("foreign.git");
	for repository in [&leaf, &middle, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(repository.join("file.txt"), b"content\n").unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "root");
	}
	git_allow(
		&middle,
		&["submodule", "add", "--name", "child", "../leaf", "child"],
	);
	commit(&middle, "add child");
	git_allow(
		&superproject,
		&[
			"submodule",
			"add",
			"--name",
			"parent",
			"../middle",
			"parent",
		],
	);
	commit(&superproject, "add parent");
	let clone = Command::new("git")
		.args(["clone", "-q", "--no-recurse-submodules"])
		.arg(&superproject)
		.arg(&consumer)
		.output()
		.unwrap();
	assert_success(&clone, "clone redirected-child fixture");
	let parent = gta(
		&consumer,
		true,
		&["submodule", "update", "--init", "parent"],
	);
	assert_success(&parent, "initialize parent only");

	let initialized = Command::new("git")
		.args(["init", "--bare", "-q"])
		.arg(&foreign)
		.output()
		.unwrap();
	assert_success(&initialized, "initialize unrelated bare repository");
	let module_git_dir = git_path(&consumer, "modules/parent");
	std::fs::write(
		module_git_dir.join("commondir"),
		format!("{}\n", foreign.display()),
	)
	.unwrap();
	let foreign_config = std::fs::read(foreign.join("config")).unwrap();

	let update = gta(
		&consumer,
		true,
		&["submodule", "update", "--init", "--recursive", "parent"],
	);
	assert!(!update.status.success());
	assert!(
		stderr(&update).contains("submodule repository attachment changed"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert_eq!(
		std::fs::read(foreign.join("config")).unwrap(),
		foreign_config,
		"recursive entry must reject the redirect before mutating unrelated config"
	);
	assert!(
		!consumer.join("parent/child/.git").exists(),
		"recursive update must not initialize the redirected child"
	);
	std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recursive_recovery_treats_the_recorded_owner_as_a_literal_path() {
	let root = unique_tmp("recursive-literal-recovery");
	let source = root.join("source");
	let superproject = root.join("super");
	let consumer = root.join("consumer");
	for repository in [&source, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(repository.join("file.txt"), b"content\n").unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "root");
	}
	for (name, path) in [
		("bracket", "a[bc]"),
		("sibling", "ab"),
		("target", "target"),
	] {
		git_allow(
			&superproject,
			&["submodule", "add", "--name", name, "../source", path],
		);
	}
	commit(&superproject, "add literal recovery modules");
	let clone = Command::new("git")
		.args(["clone", "-q", "--no-recurse-submodules"])
		.arg(&superproject)
		.arg(&consumer)
		.output()
		.unwrap();
	assert_success(&clone, "clone literal recovery fixture");
	let recorded = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();
	assert_success(
		&gta(&consumer, false, &["submodule", "init"]),
		"register every literal recovery module",
	);
	assert_success(
		&gta(
			&consumer,
			true,
			&["submodule", "update", ":(top,literal)a[bc]"],
		),
		"materialize the literal recovery owner",
	);
	let source_url = git(
		&consumer.join("a[bc]"),
		&["config", "--get", "remote.origin.url"],
	)
	.trim()
	.to_owned();
	let control = write_v4_recovery_intent_at(
		&consumer,
		"bracket",
		"a[bc]",
		&recorded,
		&source_url,
		"module",
	);

	let recovered = gta(
		&consumer,
		true,
		&["submodule", "update", "--recursive", "target"],
	);
	assert_success(
		&recovered,
		"recover a literal owner before the requested module",
	);
	assert!(consumer.join("a[bc]/.git").is_file());
	assert!(consumer.join("target/.git").is_file());
	assert!(
		!consumer.join("ab/.git").exists(),
		"the recorded 'a[bc]' owner must not select its glob-matching sibling"
	);
	assert!(
		!control.exists(),
		"the exact owner's recovery must complete"
	);
	std::fs::remove_dir_all(root).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn submodule_status_waits_for_each_module_config_publication() {
	let fixture = Fixture::new("status-module-config-locked");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let module = Dir::open_ambient_dir(&module_git_dir, ambient_authority()).unwrap();
	let mutation = acquire_submodule_config_mutation_lease(&module, &module_git_dir).unwrap();
	let config = module_git_dir.join("config");
	let displaced = module_git_dir.join("config.status-displaced");
	std::fs::rename(&config, &displaced).unwrap();

	let mut child = Command::new(env!("CARGO_BIN_EXE_gta"))
		.args(["-C", fixture.consumer.to_str().unwrap()])
		.args(["submodule", "status"])
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.expect("start status while the module config is displaced");
	std::thread::sleep(std::time::Duration::from_millis(250));
	let early = child.try_wait().unwrap();
	std::fs::rename(&displaced, &config).unwrap();
	drop(mutation);
	assert!(
		early.is_none(),
		"status observed the module's transient absent-config window"
	);
	let status = child.wait_with_output().unwrap();
	assert_success(&status, "status after module config publication");
	assert_eq!(stdout(&status), format!(" {} modules/one\n", fixture.old));
}

#[cfg(unix)]
#[test]
fn submodule_status_rejects_a_module_repository_replaced_while_waiting() {
	let fixture = Fixture::new("status-module-replaced-while-waiting");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let displaced = module_git_dir.with_file_name("one.status-displaced");
	let module = Dir::open_ambient_dir(&module_git_dir, ambient_authority()).unwrap();
	let mutation = acquire_submodule_config_mutation_lease(&module, &module_git_dir).unwrap();

	let mut child = Command::new(env!("CARGO_BIN_EXE_gta"))
		.args(["-C", fixture.consumer.to_str().unwrap()])
		.args(["submodule", "status"])
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.expect("start status behind the module config guard");
	std::thread::sleep(std::time::Duration::from_millis(250));
	assert!(
		child.try_wait().unwrap().is_none(),
		"status did not wait for module serialization"
	);
	std::fs::rename(&module_git_dir, &displaced).unwrap();
	std::fs::create_dir(&module_git_dir).unwrap();
	drop(mutation);

	let status = child.wait_with_output().unwrap();
	assert!(
		!status.status.success(),
		"status must reject a replaced module repository"
	);
	assert!(
		stderr(&status).contains("submodule repository for 'one' is corrupt"),
		"unexpected error: {}",
		stderr(&status)
	);
}

#[cfg(unix)]
#[test]
fn nested_submodule_status_rejects_a_checkout_retired_while_waiting() {
	let fixture = Fixture::new("nested-status-retired-while-waiting");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let mount = fixture.consumer.join("modules/one");
	let retained = fixture.consumer.join("modules/.one-retired");
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let module = Dir::open_ambient_dir(&module_git_dir, ambient_authority()).unwrap();
	let mutation = acquire_submodule_config_mutation_lease(&module, &module_git_dir).unwrap();

	let mut child = Command::new(env!("CARGO_BIN_EXE_gta"))
		.args(["-C", mount.to_str().unwrap()])
		.args(["submodule", "status"])
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.expect("start nested status behind the module config guard");
	std::thread::sleep(std::time::Duration::from_millis(250));
	assert!(
		child.try_wait().unwrap().is_none(),
		"nested status did not wait for module serialization"
	);
	std::fs::rename(&mount, &retained).unwrap();
	std::fs::create_dir(&mount).unwrap();
	drop(mutation);

	let status = child.wait_with_output().unwrap();
	assert!(
		!status.status.success(),
		"nested status must not continue through the retired checkout"
	);
	assert!(
		stderr(&status).contains("worktree changed while waiting for repository setup"),
		"unexpected error: {}",
		stderr(&status)
	);
	assert_eq!(std::fs::read_dir(&mount).unwrap().count(), 0);
	assert!(retained.join("file.txt").is_file());
}

#[test]
fn deinit_clears_the_mount_but_retains_the_repository_for_reattachment() {
	let fixture = Fixture::new("deinit-reattach");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&deinit, "submodule deinit");
	assert_eq!(stdout(&deinit), "Cleared directory 'modules/one'\n");
	assert!(
		stderr(&deinit).contains("Submodule 'one' unregistered"),
		"unregistration is reported: {}",
		stderr(&deinit)
	);
	assert!(fixture.consumer.join("modules/one").is_dir());
	assert_eq!(
		std::fs::read_dir(fixture.consumer.join("modules/one"))
			.unwrap()
			.count(),
		0,
		"the public mount remains as an empty directory"
	);
	assert!(module_git_dir.is_dir(), "the retained repository survives");
	assert_eq!(
		std::fs::read_to_string(retired_checkout(&module_git_dir).join("file.txt")).unwrap(),
		"old\n",
		"the displaced checkout is retained losslessly"
	);
	assert!(
		!std::fs::read_to_string(module_git_dir.join("config"))
			.unwrap()
			.lines()
			.any(|line| line.trim_start().starts_with("worktree =")),
		"the retained repository is detached from the removed worktree"
	);
	assert!(
		!std::fs::read_to_string(fixture.consumer.join(".git/config"))
			.unwrap()
			.contains("[submodule \"one\"]"),
		"the writable local registration is removed"
	);

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "reattach retained repository");
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"old\n"
	);
}

#[cfg(any(unix, windows))]
#[test]
fn deinit_refuses_a_busy_module_config_before_mutating_any_namespace() {
	let fixture = Fixture::new("deinit-module-config-locked");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let module = Dir::open_ambient_dir(&module_git_dir, ambient_authority()).unwrap();
	let mutation = acquire_submodule_config_mutation_lease(&module, &module_git_dir).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(!refused.status.success(), "a busy module config must fail");
	assert!(
		stderr(&refused).contains("submodule update is already running"),
		"unexpected error: {}",
		stderr(&refused)
	);
	assert!(fixture.consumer.join("modules/one/file.txt").is_file());
	assert!(
		std::fs::read_to_string(fixture.consumer.join(".git/config"))
			.unwrap()
			.contains("[submodule \"one\"]")
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());

	drop(mutation);
	let retry = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&retry, "deinit after the module config writer exits");
}

#[cfg(any(unix, windows))]
#[test]
fn deinit_refuses_a_module_repository_with_pending_nested_recovery() {
	let fixture = Fixture::new("deinit-module-nested-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let nested_control = module_git_dir.join("gitana-submodule-deinit");
	std::fs::create_dir(&nested_control).unwrap();
	let super_config = fixture.consumer.join(".git/config");
	let super_before = std::fs::read(&super_config).unwrap();
	let module_config = module_git_dir.join("config");
	let module_before = std::fs::read(&module_config).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"nested recovery must block parent deinit"
	);
	assert!(
		stderr(&refused).contains("pending submodule deinit recovery in module 'one'"),
		"unexpected nested recovery error: {}",
		stderr(&refused)
	);
	assert!(fixture.consumer.join("modules/one/file.txt").is_file());
	assert_eq!(std::fs::read(&super_config).unwrap(), super_before);
	assert_eq!(std::fs::read(&module_config).unwrap(), module_before);
	assert!(
		!git_path(&fixture.consumer, "gitana-submodule-deinit").exists(),
		"parent deinit must not publish an intent"
	);

	std::fs::remove_dir(&nested_control).unwrap();
	let retry = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&retry, "deinit after nested recovery is cleared");
}

#[cfg(any(unix, windows))]
#[test]
fn update_refuses_a_busy_retained_module_config_before_attachment() {
	let fixture = Fixture::new("update-module-config-locked");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	assert_success(
		&gta(
			&fixture.consumer,
			false,
			&["submodule", "deinit", "modules/one"],
		),
		"deinit retained repository",
	);
	assert_success(
		&gta(
			&fixture.consumer,
			false,
			&["submodule", "init", "modules/one"],
		),
		"register retained repository",
	);

	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let module = Dir::open_ambient_dir(&module_git_dir, ambient_authority()).unwrap();
	let mutation = acquire_submodule_config_mutation_lease(&module, &module_git_dir).unwrap();
	let mount = fixture.consumer.join("modules/one");

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "update", "modules/one"],
	);
	assert!(!refused.status.success(), "a busy module config must fail");
	assert!(
		stderr(&refused).contains("submodule update is already running"),
		"unexpected error: {}",
		stderr(&refused)
	);
	assert_eq!(std::fs::read_dir(&mount).unwrap().count(), 0);
	assert!(
		!std::fs::read_to_string(module_git_dir.join("config"))
			.unwrap()
			.lines()
			.any(|line| line.trim_start().starts_with("worktree =")),
		"the busy retained repository must stay detached"
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-update").exists());

	drop(mutation);
	let retry = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "modules/one"],
	);
	assert_success(&retry, "reattach after the module config writer exits");
	assert!(mount.join(".git").is_file());
	assert!(mount.join("file.txt").is_file());
	assert!(
		std::fs::read_to_string(module_git_dir.join("config"))
			.unwrap()
			.lines()
			.any(|line| line.trim_start().starts_with("worktree =")),
		"retry must restore the retained repository attachment"
	);
}

#[test]
fn deinit_refuses_visible_changes_but_force_retires_them() {
	let fixture = Fixture::new("deinit-force");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	std::fs::write(fixture.consumer.join("modules/one/file.txt"), b"changed\n").unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"a visible modification must fail"
	);
	assert!(
		stderr(&refused).contains(
			"contains local modifications; use --force to deinitialize while retaining the checkout and its local changes"
		),
		"unexpected error: {}",
		stderr(&refused)
	);
	assert_eq!(
		std::fs::read_to_string(fixture.consumer.join("modules/one/file.txt")).unwrap(),
		"changed\n"
	);

	let forced = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "modules/one"],
	);
	assert_success(&forced, "forced submodule deinit");
	assert_eq!(
		std::fs::read_dir(fixture.consumer.join("modules/one"))
			.unwrap()
			.count(),
		0
	);
	assert_eq!(
		std::fs::read_to_string(
			retired_checkout(&git_path(&fixture.consumer, "modules/one")).join("file.txt")
		)
		.unwrap(),
		"changed\n"
	);
}

#[test]
fn deinit_refuses_a_clean_checkout_at_a_different_commit_without_force() {
	let fixture = Fixture::new("deinit-clean-wrong-head");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	std::fs::write(module.join("file.txt"), b"local commit\n").unwrap();
	git_ok(&module, &["add", "file.txt"]);
	commit(&module, "local module commit");
	assert_ne!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
	assert!(git(&module, &["status", "--porcelain"]).trim().is_empty());

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"a divergent clean HEAD must fail"
	);
	assert!(stderr(&refused).contains("contains local modifications"));
	assert!(module.join("file.txt").is_file());

	let forced = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "modules/one"],
	);
	assert_success(
		&forced,
		"force may bypass the divergent HEAD cleanliness proof",
	);
}

#[test]
fn deinit_retires_ignored_content_without_force_and_requires_a_selection() {
	let fixture = Fixture::new("deinit-ignored");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	std::fs::create_dir_all(module_git_dir.join("info")).unwrap();
	std::fs::write(module_git_dir.join("info/exclude"), b"ignored\n").unwrap();
	std::fs::write(fixture.consumer.join("modules/one/ignored"), b"ignored\n").unwrap();

	let missing = gta(&fixture.consumer, false, &["submodule", "deinit"]);
	assert!(
		!missing.status.success(),
		"deinit must require --all or paths"
	);

	let deinit = gta(&fixture.consumer, false, &["submodule", "deinit", "--all"]);
	assert_success(&deinit, "deinit with ignored content");
	assert_eq!(
		std::fs::read_dir(fixture.consumer.join("modules/one"))
			.unwrap()
			.count(),
		0
	);
	assert_eq!(
		std::fs::read_to_string(
			retired_checkout(&git_path(&fixture.consumer, "modules/one")).join("ignored")
		)
		.unwrap(),
		"ignored\n"
	);
}

#[test]
fn deinit_all_retires_an_unpublished_control_after_gitlink_removal() {
	let fixture = Fixture::new("deinit-unpublished-control");
	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	std::fs::create_dir(&control).unwrap();
	std::fs::write(control.join("intent.lock"), b"partial intent").unwrap();
	git_ok(
		&fixture.consumer,
		&["update-index", "--force-remove", "modules/one"],
	);

	let blocked = gta(
		&fixture.consumer,
		false,
		&["config", "--local", "test.pending", "blocked"],
	);
	assert!(
		!blocked.status.success(),
		"unpublished control state must block config mutation"
	);
	assert!(
		stderr(&blocked).contains("pending submodule deinit"),
		"unexpected error: {}",
		stderr(&blocked)
	);

	let deinit = gta(&fixture.consumer, false, &["submodule", "deinit", "--all"]);
	assert_success(&deinit, "retire unpublished control state");
	assert!(!control.exists(), "the active control name must be retired");

	let git_dir = control.parent().unwrap();
	let retired = std::fs::read_dir(git_dir)
		.unwrap()
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| {
			path
				.file_name()
				.and_then(|name| name.to_str())
				.is_some_and(|name| name.starts_with(".gitana-submodule-deinit-retired."))
		})
		.collect::<Vec<_>>();
	assert_eq!(
		retired.len(),
		1,
		"the incomplete control state is preserved"
	);
	assert_eq!(
		std::fs::read(retired[0].join("intent.lock")).unwrap(),
		b"partial intent"
	);

	let write = gta(
		&fixture.consumer,
		false,
		&["config", "--local", "test.pending", "recovered"],
	);
	assert_success(&write, "config mutation after control retirement");
	assert_eq!(
		git(&fixture.consumer, &["config", "--get", "test.pending"]).trim(),
		"recovered"
	);
}

#[cfg(unix)]
#[test]
fn deinit_all_recovers_an_active_intent_after_gitlink_removal() {
	let fixture = Fixture::new("deinit-intent-removed-gitlink");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let control = write_prepared_deinit_intent(&fixture, "removed-gitlink");
	git_ok(
		&fixture.consumer,
		&["update-index", "--force-remove", "modules/one"],
	);

	let recovered = gta(&fixture.consumer, false, &["submodule", "deinit", "--all"]);
	assert_success(&recovered, "recover after removing the recorded gitlink");
	assert_eq!(stdout(&recovered), "Cleared directory 'modules/one'\n");
	assert!(!control.exists(), "the active recovery journal is retired");
	assert_eq!(
		std::fs::read_dir(fixture.consumer.join("modules/one"))
			.unwrap()
			.count(),
		0
	);
	assert!(retired_checkout(&git_path(&fixture.consumer, "modules/one")).is_dir());
}

#[cfg(unix)]
#[test]
fn deinit_path_recovers_the_persisted_gitlink_after_the_index_oid_changes() {
	let fixture = Fixture::new("deinit-intent-changed-gitlink");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let control = write_prepared_deinit_intent(&fixture, "changed-gitlink");
	let next = fixture.commit_source("next\n", "next");
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{next},modules/one"),
		],
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "recover the persisted gitlink identity");
	assert_eq!(
		stdout(&recovered),
		"Cleared directory 'modules/one'\n",
		"the replacement gitlink must not start a second transition"
	);
	assert!(!control.exists(), "the active recovery journal is retired");
	assert!(retired_checkout(&git_path(&fixture.consumer, "modules/one")).is_dir());
}

#[test]
fn deinit_force_still_refuses_a_foreign_mount_marker() {
	let fixture = Fixture::new("deinit-foreign-force");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let marker = fixture.consumer.join("modules/one/.git");
	std::fs::write(&marker, b"gitdir: ../../foreign\n").unwrap();

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "modules/one"],
	);
	assert!(!deinit.status.success(), "force must not bypass ownership");
	assert!(
		stderr(&deinit).contains("foreign or non-empty content"),
		"unexpected error: {}",
		stderr(&deinit)
	);
	assert_eq!(std::fs::read(&marker).unwrap(), b"gitdir: ../../foreign\n");
}

#[test]
fn deinit_force_still_validates_the_module_repository_format() {
	let fixture = Fixture::new("deinit-force-format");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = git_path(&fixture.consumer, "modules/one");
	git_ok(
		&module,
		&[
			"config",
			"--file",
			"config",
			"core.repositoryFormatVersion",
			"1",
		],
	);
	git_ok(
		&module,
		&[
			"config",
			"--file",
			"config",
			"extensions.objectFormat",
			"sha256",
		],
	);
	let module_before = std::fs::read(module.join("config")).unwrap();
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "modules/one"],
	);
	assert!(
		!deinit.status.success(),
		"force must not bypass repository validation"
	);
	assert!(fixture.consumer.join("modules/one/.git").is_file());
	assert_eq!(std::fs::read(module.join("config")).unwrap(), module_before);
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());
}

#[test]
fn deinit_validates_an_already_unmounted_module_repository() {
	let fixture = Fixture::new("deinit-unmounted-format");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let mount = fixture.consumer.join("modules/one");
	std::fs::remove_dir_all(&mount).unwrap();
	std::fs::create_dir(&mount).unwrap();
	let module = git_path(&fixture.consumer, "modules/one");
	git_ok(
		&module,
		&[
			"config",
			"--file",
			"config",
			"core.repositoryFormatVersion",
			"1",
		],
	);
	git_ok(
		&module,
		&[
			"config",
			"--file",
			"config",
			"extensions.objectFormat",
			"sha256",
		],
	);
	let module_before = std::fs::read(module.join("config")).unwrap();
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!deinit.status.success(),
		"unmounted modules still require repository validation"
	);
	assert_eq!(std::fs::read_dir(&mount).unwrap().count(), 0);
	assert_eq!(std::fs::read(module.join("config")).unwrap(), module_before);
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());
}

#[test]
fn deinit_all_preflights_every_checkout_before_clearing_the_first() {
	let fixture = Fixture::new("deinit-preflight-all");
	add_second_module_mapping(&fixture);
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initialize both modules",
	);
	std::fs::write(fixture.consumer.join("modules/two/file.txt"), b"changed\n").unwrap();

	let deinit = gta(&fixture.consumer, false, &["submodule", "deinit", "--all"]);
	assert!(
		!deinit.status.success(),
		"the dirty second module must fail"
	);
	assert!(
		fixture.consumer.join("modules/one/.git").is_file(),
		"complete preflight must preserve the clean first module"
	);
	assert!(
		fixture.consumer.join("modules/two/.git").is_file(),
		"the dirty module must remain mounted"
	);
}

#[test]
fn deinit_all_preflights_every_module_config_attachment_before_mutation() {
	let fixture = Fixture::new("deinit-preflight-config-all");
	add_second_module_mapping(&fixture);
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initialize both modules",
	);
	let second_git_dir = git_path(&fixture.consumer, "modules/two");
	git_ok(
		&second_git_dir,
		&["config", "--file", "config", "core.worktree", "sentinel"],
	);
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "--all"],
	);
	assert!(!deinit.status.success(), "the foreign attachment must fail");
	assert!(stderr(&deinit).contains("core.worktree points to 'sentinel'"));
	assert!(fixture.consumer.join("modules/one/.git").is_file());
	assert!(fixture.consumer.join("modules/two/.git").is_file());
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before
	);
	assert!(
		!git_path(&fixture.consumer, "gitana-submodule-deinit").exists(),
		"batch preflight must publish no intent"
	);
}

#[cfg(unix)]
#[test]
fn deinit_all_rejects_a_shared_config_target_inside_a_later_checkout_before_mutation() {
	let fixture = Fixture::new("deinit-preflight-shared-config-containment-all");
	add_second_module_mapping(&fixture);
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initialize both modules",
	);
	let config = fixture.consumer.join(".git/config");
	let contained_target = fixture.consumer.join("modules/two/super-config-real");
	std::fs::rename(&config, &contained_target).unwrap();
	std::os::unix::fs::symlink("../modules/two/super-config-real", &config).unwrap();
	let super_before = std::fs::read(&contained_target).unwrap();
	let first_module_config = git_path(&fixture.consumer, "modules/one/config");
	let second_module_config = git_path(&fixture.consumer, "modules/two/config");
	let first_before = std::fs::read(&first_module_config).unwrap();
	let second_before = std::fs::read(&second_module_config).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "--all"],
	);
	assert!(
		!refused.status.success(),
		"contained config target must fail"
	);
	assert!(
		stderr(&refused).contains("superproject config target is inside the selected checkout"),
		"unexpected diagnostic: {}",
		stderr(&refused),
	);
	assert!(fixture.consumer.join("modules/one/.git").is_file());
	assert!(fixture.consumer.join("modules/two/.git").is_file());
	assert!(fixture.consumer.join("modules/one/file.txt").is_file());
	assert!(fixture.consumer.join("modules/two/file.txt").is_file());
	assert_eq!(std::fs::read(&first_module_config).unwrap(), first_before);
	assert_eq!(std::fs::read(&second_module_config).unwrap(), second_before);
	assert_eq!(std::fs::read(&contained_target).unwrap(), super_before);
	assert!(
		std::fs::symlink_metadata(&config)
			.unwrap()
			.file_type()
			.is_symlink(),
		"preflight must preserve the shared config symlink",
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());

	let external_target = fixture.root.join("external-superproject-config");
	std::fs::remove_file(&config).unwrap();
	std::fs::rename(&contained_target, &external_target).unwrap();
	std::os::unix::fs::symlink(&external_target, &config).unwrap();
	assert_success(
		&gta(
			&fixture.consumer,
			false,
			&["submodule", "deinit", "--force", "--all"],
		),
		"deinit through external shared config symlink",
	);
	assert_eq!(
		std::fs::read_dir(fixture.consumer.join("modules/one"))
			.unwrap()
			.count(),
		0,
	);
	assert_eq!(
		std::fs::read_dir(fixture.consumer.join("modules/two"))
			.unwrap()
			.count(),
		0,
	);
}

#[cfg(unix)]
#[test]
fn deinit_all_rejects_a_module_config_target_inside_another_checkout_before_mutation() {
	let fixture = Fixture::new("deinit-preflight-cross-module-config-containment");
	add_second_module_mapping(&fixture);
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initialize both modules",
	);
	let first_config = git_path(&fixture.consumer, "modules/one/config");
	let second_config = git_path(&fixture.consumer, "modules/two/config");
	let contained_target = fixture.consumer.join("modules/two/one-config-real");
	std::fs::rename(&first_config, &contained_target).unwrap();
	std::os::unix::fs::symlink(&contained_target, &first_config).unwrap();
	let first_before = std::fs::read(&contained_target).unwrap();
	let second_before = std::fs::read(&second_config).unwrap();
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "--all"],
	);
	assert!(
		!refused.status.success(),
		"cross-module config target must fail"
	);
	assert!(
		stderr(&refused).contains("module config target is inside the selected checkout"),
		"unexpected diagnostic: {}",
		stderr(&refused),
	);
	assert!(fixture.consumer.join("modules/one/.git").is_file());
	assert!(fixture.consumer.join("modules/two/.git").is_file());
	assert_eq!(std::fs::read(&contained_target).unwrap(), first_before);
	assert_eq!(std::fs::read(&second_config).unwrap(), second_before);
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());
}

#[test]
fn deinit_all_rejects_a_module_include_inside_another_selected_checkout() {
	let fixture = Fixture::new("deinit-cross-module-config-include");
	add_second_module_mapping(&fixture);
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initialize both modules",
	);
	let first_config = git_path(&fixture.consumer, "modules/one/config");
	let second_config = git_path(&fixture.consumer, "modules/two/config");
	let included = fixture.consumer.join("modules/two/one-effective.inc");
	std::fs::write(&included, "[core]\n\texcludesFile = absent/ignore\n").unwrap();
	let mut first_before = std::fs::read_to_string(&first_config).unwrap();
	first_before.push_str(&format!("[include]\n\tpath = {}\n", included.display()));
	std::fs::write(&first_config, &first_before).unwrap();
	let second_before = std::fs::read(&second_config).unwrap();
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "--all"],
	);
	assert!(!refused.status.success(), "cross-module include must fail");
	assert!(
		stderr(&refused).contains("effective config input"),
		"unexpected diagnostic: {}",
		stderr(&refused)
	);
	assert!(fixture.consumer.join("modules/one/.git").is_file());
	assert!(fixture.consumer.join("modules/two/.git").is_file());
	assert_eq!(
		std::fs::read_to_string(&first_config).unwrap(),
		first_before
	);
	assert_eq!(std::fs::read(&second_config).unwrap(), second_before);
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());
}

#[cfg(unix)]
#[test]
fn deinit_rejects_aliased_module_and_superproject_config_targets_before_mutation() {
	let fixture = Fixture::new("deinit-aliased-config-targets");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_config = git_path(&fixture.consumer, "modules/one/config");
	let super_config = fixture.consumer.join(".git/config");
	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	let shared_config = fixture.root.join("aliased-config");
	let mut shared_bytes = std::fs::read(&module_config).unwrap();
	shared_bytes.extend_from_slice(
		format!(
			"\n[submodule \"one\"]\n\turl = {}\n\tactive = true\n",
			fixture.source.display()
		)
		.as_bytes(),
	);
	std::fs::write(&shared_config, &shared_bytes).unwrap();
	std::fs::remove_file(&module_config).unwrap();
	std::fs::remove_file(&super_config).unwrap();
	std::os::unix::fs::symlink(&shared_config, &module_config).unwrap();
	std::os::unix::fs::symlink(&shared_config, &super_config).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"aliased config targets must fail"
	);
	assert!(
		stderr(&refused)
			.contains("deinit config targets for module 'one' and superproject resolve to the same file"),
		"unexpected diagnostic: {}",
		stderr(&refused),
	);
	assert_eq!(std::fs::read(&shared_config).unwrap(), shared_bytes);
	assert!(fixture.consumer.join("modules/one/.git").is_file());
	assert!(fixture.consumer.join("modules/one/file.txt").is_file());
	assert!(!control.exists(), "alias preflight must publish no intent");
}

#[test]
fn deinit_rejects_every_worktree_local_core_worktree_override() {
	for (tag, use_equivalent_override) in [
		("deinit-worktree-config-foreign", false),
		("deinit-worktree-config-equivalent", true),
	] {
		let fixture = Fixture::new(tag);
		assert_success(
			&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
			"initial update",
		);
		let module = git_path(&fixture.consumer, "modules/one");
		let expected = git(
			&module,
			&["config", "--file", "config", "--get", "core.worktree"],
		);
		git_ok(
			&module,
			&[
				"config",
				"--file",
				"config",
				"extensions.worktreeConfig",
				"true",
			],
		);
		let override_value = if use_equivalent_override {
			expected.trim()
		} else {
			"sentinel"
		};
		std::fs::write(
			module.join("config.worktree"),
			format!("[core]\n\tworktree = {override_value}\n"),
		)
		.unwrap();
		let module_before = std::fs::read(module.join("config")).unwrap();
		let worktree_before = std::fs::read(module.join("config.worktree")).unwrap();
		let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

		let deinit = gta(
			&fixture.consumer,
			false,
			&["submodule", "deinit", "--force", "modules/one"],
		);
		assert!(!deinit.status.success(), "worktree override must fail");
		assert!(
			stderr(&deinit).contains("worktree-local config defines core.worktree"),
			"unexpected error: {}",
			stderr(&deinit)
		);
		assert!(fixture.consumer.join("modules/one/.git").is_file());
		assert_eq!(std::fs::read(module.join("config")).unwrap(), module_before);
		assert_eq!(
			std::fs::read(module.join("config.worktree")).unwrap(),
			worktree_before
		);
		assert_eq!(
			std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
			super_before
		);
		assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());
	}
}

#[test]
fn deinit_rejects_an_included_core_worktree_that_would_survive_the_base_edit() {
	let fixture = Fixture::new("deinit-included-worktree");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = git_path(&fixture.consumer, "modules/one");
	let expected = git(
		&module,
		&["config", "--file", "config", "--get", "core.worktree"],
	);
	std::fs::write(
		module.join("attachment.inc"),
		format!("[core]\n\tworktree = {}\n", expected.trim()),
	)
	.unwrap();
	let mut base = std::fs::read_to_string(module.join("config")).unwrap();
	base.push_str("[include]\n\tpath = attachment.inc\n");
	std::fs::write(module.join("config"), &base).unwrap();
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "modules/one"],
	);
	assert!(!deinit.status.success(), "surviving include must fail");
	assert!(
		stderr(&deinit).contains("would still define core.worktree"),
		"unexpected error: {}",
		stderr(&deinit)
	);
	assert!(fixture.consumer.join("modules/one/.git").is_file());
	assert_eq!(
		std::fs::read_to_string(module.join("config")).unwrap(),
		base
	);
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());
}

#[test]
fn deinit_rejects_an_effective_config_include_inside_the_selected_checkout() {
	let fixture = Fixture::new("deinit-checkout-contained-include");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let checkout = fixture.consumer.join("modules/one");
	let module = git_path(&fixture.consumer, "modules/one");
	let included = checkout.join("effective.inc");
	std::fs::write(&included, "[core]\n\texcludesFile = absent/ignore\n").unwrap();
	let mut module_config = std::fs::read_to_string(module.join("config")).unwrap();
	module_config.push_str(&format!("[include]\n\tpath = {}\n", included.display()));
	std::fs::write(module.join("config"), &module_config).unwrap();
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"checkout-contained include must fail"
	);
	assert!(
		stderr(&refused).contains("effective config input"),
		"unexpected diagnostic: {}",
		stderr(&refused)
	);
	assert!(checkout.join(".git").is_file());
	assert_eq!(
		std::fs::read_to_string(module.join("config")).unwrap(),
		module_config
	);
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());
}

#[test]
fn deinit_accepts_equivalent_core_worktree_path_spellings() {
	for (tag, absolute) in [
		("deinit-worktree-absolute", true),
		("deinit-worktree-dotted", false),
	] {
		let fixture = Fixture::new(tag);
		assert_success(
			&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
			"initial update",
		);
		let module = git_path(&fixture.consumer, "modules/one");
		let current = git(
			&module,
			&["config", "--file", "config", "--get", "core.worktree"],
		);
		let spelling = if absolute {
			fixture
				.consumer
				.join("modules/one")
				.to_string_lossy()
				.into_owned()
		} else {
			format!("./{}/../one", current.trim())
		};
		git_ok(
			&module,
			&["config", "--file", "config", "core.worktree", &spelling],
		);

		let deinit = gta(
			&fixture.consumer,
			false,
			&["submodule", "deinit", "modules/one"],
		);
		assert_success(&deinit, "deinit with equivalent core.worktree spelling");
		assert_eq!(
			std::fs::read_dir(fixture.consumer.join("modules/one"))
				.unwrap()
				.count(),
			0
		);
	}
}

#[test]
fn deinit_rechecks_worktree_relative_excludes_through_the_displaced_mount() {
	let fixture = Fixture::new("deinit-relative-excludes");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let checkout = fixture.consumer.join("modules/one");
	git_ok(&checkout, &["config", "core.excludesFile", "local.ignore"]);
	std::fs::write(
		checkout.join("local.ignore"),
		b"local.ignore\nignored-after-swap\n",
	)
	.unwrap();
	std::fs::write(checkout.join("ignored-after-swap"), b"retained\n").unwrap();

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&deinit, "deinit with worktree-local excludes");
	let retired = retired_checkout(&git_path(&fixture.consumer, "modules/one"));
	assert_eq!(
		std::fs::read_to_string(retired.join("ignored-after-swap")).unwrap(),
		"retained\n"
	);
}

#[test]
fn deinit_rechecks_absolute_in_worktree_excludes_through_the_displaced_mount() {
	let fixture = Fixture::new("deinit-absolute-excludes");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let checkout = fixture.consumer.join("modules/one");
	let excludes = checkout.join("local.ignore");
	git_ok(
		&checkout,
		&["config", "core.excludesFile", excludes.to_str().unwrap()],
	);
	std::fs::write(&excludes, b"local.ignore\nignored-after-swap\n").unwrap();
	std::fs::write(checkout.join("ignored-after-swap"), b"retained\n").unwrap();

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&deinit, "deinit with absolute worktree-local excludes");
	let retired = retired_checkout(&git_path(&fixture.consumer, "modules/one"));
	assert_eq!(
		std::fs::read_to_string(retired.join("ignored-after-swap")).unwrap(),
		"retained\n"
	);
}

#[test]
fn deinit_treats_a_missing_relative_excludes_parent_as_absent() {
	let fixture = Fixture::new("deinit-missing-excludes-parent");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let checkout = fixture.consumer.join("modules/one");
	git_ok(&checkout, &["config", "core.excludesFile", "absent/ignore"]);
	assert!(!checkout.join("absent").exists());

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&deinit, "deinit with an absent excludes file");
}

#[test]
fn deinit_rejects_a_relative_excludes_directory() {
	let fixture = Fixture::new("deinit-excludes-directory");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let checkout = fixture.consumer.join("modules/one");
	git_ok(&checkout, &["config", "core.excludesFile", "exclude-dir"]);
	std::fs::create_dir(checkout.join("exclude-dir")).unwrap();

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(!deinit.status.success(), "an excludes directory must fail");
	assert!(
		stderr(&deinit).contains("cannot use") && stderr(&deinit).contains("as an exclude file"),
		"unexpected error: {}",
		stderr(&deinit)
	);
	assert!(checkout.join("file.txt").is_file());
}

#[cfg(unix)]
#[test]
fn deinit_recovery_refuses_content_created_in_the_public_empty_mount() {
	let fixture = Fixture::new("deinit-public-mount-race");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.public-mount-race";
	let retired_name = ".gitana-submodule-deinit-retired.public-mount-race";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-public-mount-race-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);
	std::fs::write(target.join("concurrent"), b"foreign\n").unwrap();
	let module_before = std::fs::read(module_git_dir.join("config")).unwrap();
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"concurrent public content must fail"
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("config")).unwrap(),
		module_before
	);
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before
	);
	assert!(git_path(&fixture.consumer, "gitana-submodule-deinit").is_dir());

	std::fs::remove_file(target.join("concurrent")).unwrap();
	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "retry after removing public obstruction");
}

#[cfg(unix)]
#[test]
fn deinit_recovery_rechecks_the_public_mount_after_config_publication() {
	let fixture = Fixture::new("deinit-terminal-mount-race");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.terminal-mount-race";
	let retired_name = ".gitana-submodule-deinit-retired.terminal-mount-race";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-terminal-mount-race-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);
	let module_publication =
		publish_deinit_config_for_test(&module_git_dir.join("config"), "module", |config| {
			config.unset("core", None, "worktree");
		});
	let super_publication =
		publish_deinit_config_for_test(&fixture.consumer.join(".git/config"), "super", |config| {
			config.remove_subsection("submodule", "one");
		});
	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	let mut intent: serde_json::Value =
		serde_json::from_slice(&std::fs::read(control.join("intent.json")).unwrap()).unwrap();
	intent["phase"] = serde_json::json!("super_config_applied");
	intent["module_publication"] = module_publication;
	intent["super_publication"] = super_publication;
	std::fs::write(
		control.join("intent.json"),
		serde_json::to_vec(&intent).unwrap(),
	)
	.unwrap();
	std::fs::write(target.join("concurrent"), b"foreign\n").unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"terminal public content must fail"
	);
	assert!(control.is_dir(), "terminal failure must retain recovery");
	assert!(
		!std::fs::read_to_string(module_git_dir.join("config"))
			.unwrap()
			.contains("worktree")
	);
	assert!(
		!std::fs::read_to_string(fixture.consumer.join(".git/config"))
			.unwrap()
			.contains("[submodule \"one\"]")
	);

	std::fs::remove_file(target.join("concurrent")).unwrap();
	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "terminal mount retry");
}

#[test]
fn pending_deinit_recovery_blocks_mutations_but_not_status() {
	let fixture = Fixture::new("deinit-recovery-routing");
	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	std::fs::create_dir(&control).unwrap();

	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert!(
		!init.status.success(),
		"init must not bypass deinit recovery"
	);
	assert!(stderr(&init).contains("must be completed with 'gta submodule deinit'"));

	let update = gta(&fixture.consumer, false, &["submodule", "update"]);
	assert!(
		!update.status.success(),
		"update must not bypass deinit recovery"
	);
	assert!(stderr(&update).contains("must be completed with 'gta submodule deinit'"));

	let modules_before = std::fs::read(fixture.consumer.join(".gitmodules")).unwrap();
	let set_branch = gta(
		&fixture.consumer,
		false,
		&["submodule", "set-branch", "--branch=main", "modules/one"],
	);
	assert!(
		!set_branch.status.success(),
		"set-branch must not bypass deinit recovery"
	);
	assert!(stderr(&set_branch).contains("must be completed with 'gta submodule deinit'"));
	assert_eq!(
		std::fs::read(fixture.consumer.join(".gitmodules")).unwrap(),
		modules_before
	);

	let status = gta(&fixture.consumer, false, &["submodule", "status"]);
	assert_success(&status, "read-only status during recovery");
	assert_eq!(stdout(&status), format!("-{} modules/one\n", fixture.old));
}

#[cfg(unix)]
#[test]
fn linked_worktree_deinit_intent_blocks_shared_config_mutations() {
	use std::os::unix::fs::MetadataExt as _;

	let fixture = Fixture::new("deinit-linked-mutation-gate");
	let owner = fixture.root.join("deinit-owner");
	let sibling = fixture.root.join("deinit-sibling");
	git_ok(
		&fixture.consumer,
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"deinit-mutation-owner",
			owner.to_str().unwrap(),
		],
	);
	git_ok(
		&fixture.consumer,
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"deinit-mutation-sibling",
			sibling.to_str().unwrap(),
		],
	);
	assert_success(
		&gta(&owner, true, &["submodule", "update", "--init"]),
		"initial owner update",
	);
	git_ok(&owner, &["config", "--remove-section", "submodule.one"]);

	let module_git_dir = git_path(&owner, "modules/one");
	let parent = owner.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.linked-mutation-gate";
	let retired_name = ".gitana-submodule-deinit-retired.linked-mutation-gate";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-linked-mutation-gate-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4_at(
		&owner,
		&fixture.old,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);

	let shared_config = git_path(&owner, "config");
	let before = std::fs::read(&shared_config).unwrap();
	let before_metadata = std::fs::symlink_metadata(&shared_config).unwrap();
	for args in [
		vec!["submodule", "init"],
		vec!["submodule", "update", "--init"],
		vec!["submodule", "deinit", "modules/one"],
	] {
		let refused = gta(&sibling, true, &args);
		assert!(
			!refused.status.success(),
			"sibling mutation must not bypass the owner's deinit: {args:?}"
		);
		assert!(
			stderr(&refused).contains("pending submodule deinit"),
			"unexpected error for {args:?}: {}",
			stderr(&refused)
		);
		let after_metadata = std::fs::symlink_metadata(&shared_config).unwrap();
		assert_eq!(after_metadata.dev(), before_metadata.dev());
		assert_eq!(after_metadata.ino(), before_metadata.ino());
		assert_eq!(std::fs::read(&shared_config).unwrap(), before);
	}

	let recovered = gta(&owner, false, &["submodule", "deinit", "modules/one"]);
	assert_success(&recovered, "resume the owning linked-worktree deinit");
	assert!(!git_path(&owner, "gitana-submodule-deinit").exists());
	assert!(retired.join("file.txt").is_file());
}

#[cfg(unix)]
#[test]
fn linked_worktree_config_writers_preserve_a_prepublication_deinit_intent() {
	use std::os::unix::fs::MetadataExt as _;

	let fixture = Fixture::new("deinit-linked-config-writer-gate");
	let owner = fixture.root.join("deinit-config-owner");
	let sibling = fixture.root.join("deinit-config-sibling");
	git_ok(
		&fixture.consumer,
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"deinit-config-owner",
			owner.to_str().unwrap(),
		],
	);
	git_ok(
		&fixture.consumer,
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"deinit-config-sibling",
			sibling.to_str().unwrap(),
		],
	);
	assert_success(
		&gta(&owner, true, &["submodule", "update", "--init"]),
		"initial owner update",
	);

	let module_git_dir = git_path(&owner, "modules/one");
	let parent = owner.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.config-writer-gate";
	let retired_name = ".gitana-submodule-deinit-retired.config-writer-gate";
	let prepared = parent.join(prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	write_deinit_intent_v4_at(
		&owner,
		&fixture.old,
		"prepared",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);

	let shared_config = git_path(&owner, "config");
	let before = std::fs::read(&shared_config).unwrap();
	let before_metadata = std::fs::symlink_metadata(&shared_config).unwrap();

	assert_success(
		&gta(
			&sibling,
			true,
			&["config", "--local", "--get", "submodule.one.url"],
		),
		"serialized config read during recovery",
	);
	assert_success(
		&gta(&sibling, true, &["remote"]),
		"serialized remote read during recovery",
	);
	assert_success(
		&gta(&sibling, true, &["sparse-checkout", "reapply"]),
		"sparse reapply without a shared-config write",
	);

	for args in [
		vec!["config", "--local", "test.pending", "value"],
		vec![
			"remote",
			"add",
			"blocked",
			"https://example.invalid/blocked",
		],
		vec!["sparse-checkout", "init"],
	] {
		let refused = gta(&sibling, true, &args);
		assert!(
			!refused.status.success(),
			"shared-config writer must not bypass the owner's deinit: {args:?}"
		);
		assert!(
			stderr(&refused).contains("pending submodule deinit"),
			"unexpected error for {args:?}: {}",
			stderr(&refused)
		);
		let after_metadata = std::fs::symlink_metadata(&shared_config).unwrap();
		assert_eq!(after_metadata.dev(), before_metadata.dev());
		assert_eq!(after_metadata.ino(), before_metadata.ino());
		assert_eq!(std::fs::read(&shared_config).unwrap(), before);
	}

	let recovered = gta(&owner, false, &["submodule", "deinit", "modules/one"]);
	assert_success(&recovered, "resume the prepublication deinit");
	assert!(!git_path(&owner, "gitana-submodule-deinit").exists());
}

#[cfg(unix)]
#[test]
fn deinit_resumes_a_durable_post_swap_intent() {
	use std::os::unix::fs::MetadataExt as _;

	let fixture = Fixture::new("deinit-resume");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let module_config_path = module_git_dir.join("config");
	let super_config_path = fixture.consumer.join(".git/config");
	let core_worktree = git(&module_git_dir, &["config", "--get", "core.worktree"])
		.trim()
		.to_owned();
	let module_transition = deinit_transition(&module_config_path, |config| {
		config.unset("core", None, "worktree");
	});
	let super_transition = deinit_transition(&super_config_path, |config| {
		config.remove_subsection("submodule", "one");
	});

	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.recovery-test";
	let retired_name = ".gitana-submodule-deinit-retired.recovery-test";
	let prepared = parent.join(prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-test-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();

	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	std::fs::create_dir(&control).unwrap();
	std::fs::write(
		control.join("intent.json"),
		serde_json::to_vec(&serde_json::json!({
				"version": 4,
				"phase": "detached",
			"name": "one",
			"path": "modules/one",
			"recorded": fixture.old,
			"force": false,
			"core_worktree": core_worktree,
			"module_transition": module_transition,
			"module_publication": null,
			"super_transition": super_transition,
			"super_publication": null,
			"parent": "modules",
			"target": "one",
				"prepared": prepared_name,
				"displaced": prepared_name,
				"retired": retired_name,
				"rollback": null,
				"retirement_location": null,
			"mount_identity": {
				"device": mount_metadata.dev(),
				"inode": mount_metadata.ino()
			},
				"prepared_identity": {
					"device": prepared_metadata.dev(),
					"inode": prepared_metadata.ino()
				},
				"public_identity": {
					"device": prepared_metadata.dev(),
					"inode": prepared_metadata.ino()
				},
				"module_identity": {
					"device": module_metadata.dev(),
					"inode": module_metadata.ino()
				}
		}))
		.unwrap(),
	)
	.unwrap();

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "resume post-swap deinit intent");
	assert_eq!(stdout(&recovered), "Cleared directory 'modules/one'\n");
	assert!(!prepared.exists(), "the displaced checkout is retired");
	assert!(
		module_git_dir.join(retired_name).is_dir(),
		"the checkout is preserved under the recorded retirement name"
	);
	assert!(!control.exists(), "the active deinit journal is retired");
	assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn deinit_post_swap_recovery_rolls_back_a_clean_checkout_at_the_wrong_commit() {
	let fixture = Fixture::new("deinit-post-swap-wrong-head");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let checkout = fixture.consumer.join("modules/one");
	std::fs::write(checkout.join("file.txt"), b"local commit\n").unwrap();
	git_ok(&checkout, &["add", "file.txt"]);
	commit(&checkout, "local module commit");
	assert!(git(&checkout, &["status", "--porcelain"]).trim().is_empty());

	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.wrong-head";
	let retired_name = ".gitana-submodule-deinit-retired.wrong-head";
	let prepared = parent.join(prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-wrong-head-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"detached",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"post-swap divergent HEAD must fail"
	);
	assert!(stderr(&refused).contains("contains local modifications"));
	assert_eq!(
		std::fs::read_to_string(target.join("file.txt")).unwrap(),
		"local commit\n"
	);
	assert!(
		!prepared.exists(),
		"lossless rollback restores the original mount"
	);
	assert!(
		!git_path(&fixture.consumer, "gitana-submodule-deinit").exists(),
		"completed rollback retires the intent"
	);
}

#[cfg(unix)]
#[test]
fn deinit_recovery_preserves_writes_through_a_retained_file_descriptor() {
	use std::io::{Seek as _, SeekFrom, Write as _};

	let fixture = Fixture::new("deinit-retained-descriptor");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.retained-descriptor";
	let retired_name = ".gitana-submodule-deinit-retired.retained-descriptor";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	let mut retained_file = std::fs::OpenOptions::new()
		.write(true)
		.open(target.join("file.txt"))
		.unwrap();
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-retained-descriptor-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();

	retained_file.seek(SeekFrom::Start(0)).unwrap();
	retained_file.write_all(b"written after proof\n").unwrap();
	retained_file.set_len(20).unwrap();
	retained_file.sync_all().unwrap();
	write_deinit_intent_v4(
		&fixture,
		"retiring",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "resume checkout retirement");
	assert_eq!(
		std::fs::read_to_string(retired.join("file.txt")).unwrap(),
		"written after proof\n"
	);
}

#[cfg(unix)]
#[test]
fn deinit_recovery_accepts_a_recorded_cross_filesystem_sibling_retirement() {
	let fixture = Fixture::new("deinit-sibling-retirement");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.sibling-retirement";
	let retired_name = ".gitana-submodule-deinit-retired.sibling-retirement";
	let prepared = parent.join(prepared_name);
	let retired = parent.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-sibling-retirement-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("worktree_sibling"),
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "resume sibling retirement");
	assert_eq!(
		std::fs::read_to_string(retired.join("file.txt")).unwrap(),
		"old\n"
	);
	assert_eq!(std::fs::read_dir(target).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn deinit_recovery_rewrites_a_partial_journaled_module_config_reservation() {
	let fixture = Fixture::new("deinit-module-config-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.module-config";
	let retired_name = ".gitana-submodule-deinit-retired.module-config";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-module-config-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	let config_publication = module_git_dir.join(".gitana-config-prepared.recovery-test");
	std::fs::write(&config_publication, b"partial config image").unwrap();
	write_deinit_intent_v4(
		&fixture,
		"module_config_reserved",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		Some(&config_publication),
		Some("module_repository"),
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "rewrite and resume the reserved module config");
	assert!(retired.join("file.txt").is_file());
	assert!(
		!std::fs::read_to_string(fixture.consumer.join(".git/config"))
			.unwrap()
			.contains("[submodule \"one\"]")
	);
}

#[cfg(unix)]
#[test]
fn ordinary_submodule_status_restores_a_displaced_module_config_before_validation() {
	let fixture = Fixture::new("deinit-displaced-module-config");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.displaced-module-config";
	let retired_name = ".gitana-submodule-deinit-retired.displaced-module-config";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-displaced-module-config-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);
	let module_publication = prepare_displaced_deinit_config_for_test(
		&module_git_dir.join("config"),
		"module-displaced",
		|config| {
			config.unset("core", None, "worktree");
		},
	);
	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	let mut intent: serde_json::Value =
		serde_json::from_slice(&std::fs::read(control.join("intent.json")).unwrap()).unwrap();
	intent["phase"] = serde_json::json!("module_config_prepared");
	intent["module_publication"] = module_publication;
	std::fs::write(
		control.join("intent.json"),
		serde_json::to_vec(&intent).unwrap(),
	)
	.unwrap();
	assert!(!module_git_dir.join("config").exists());
	let status = gta(&fixture.consumer, false, &["submodule", "status"]);
	assert_success(
		&status,
		"submodule status after restoring the displaced module config",
	);
	assert!(module_git_dir.join("config").is_file());
	assert!(
		control.is_dir(),
		"setup restoration must not advance the deinit intent"
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "restore and resume displaced module config");
	assert!(!control.exists());
	assert!(module_git_dir.join("config").is_file());
	assert!(retired.join("file.txt").is_file());
}

#[cfg(unix)]
#[test]
fn ordinary_command_setup_restores_a_displaced_superproject_config_without_advancing_deinit() {
	let fixture = Fixture::new("deinit-displaced-super-config");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.displaced-super-config";
	let retired_name = ".gitana-submodule-deinit-retired.displaced-super-config";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-displaced-super-config-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);
	let module_publication =
		publish_deinit_config_for_test(&module_git_dir.join("config"), "module-setup", |config| {
			config.unset("core", None, "worktree");
		});
	let super_config = fixture.consumer.join(".git/config");
	let super_publication =
		prepare_displaced_deinit_config_for_test(&super_config, "super-displaced", |config| {
			config.remove_subsection("submodule", "one");
		});
	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	let mut intent: serde_json::Value =
		serde_json::from_slice(&std::fs::read(control.join("intent.json")).unwrap()).unwrap();
	intent["phase"] = serde_json::json!("super_config_prepared");
	intent["module_publication"] = module_publication;
	intent["super_publication"] = super_publication;
	std::fs::write(
		control.join("intent.json"),
		serde_json::to_vec(&intent).unwrap(),
	)
	.unwrap();
	assert!(!super_config.exists());

	let status = gta(&fixture.consumer, false, &["status"]);
	assert_success(&status, "ordinary status after restoring the setup config");
	assert!(super_config.is_file());
	assert!(
		control.is_dir(),
		"status must not advance the deinit intent"
	);
	let init = gta(&fixture.consumer, false, &["submodule", "init"]);
	assert!(
		!init.status.success(),
		"init must still report pending recovery"
	);
	assert!(stderr(&init).contains("must be completed with 'gta submodule deinit'"));

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(
		&recovered,
		"resume after setup restored superproject config",
	);
	assert!(!control.exists());
	assert!(retired.join("file.txt").is_file());
}

#[cfg(unix)]
#[test]
fn worktree_list_restores_a_displaced_symlinked_superproject_config() {
	use std::os::unix::fs::symlink;

	let fixture = Fixture::new("worktree-list-displaced-symlinked-super-config");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.worktree-list-symlink";
	let retired_name = ".gitana-submodule-deinit-retired.worktree-list-symlink";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-worktree-list-symlink-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);
	let module_publication =
		publish_deinit_config_for_test(&module_git_dir.join("config"), "module-list", |config| {
			config.unset("core", None, "worktree");
		});

	let super_config = fixture.consumer.join(".git/config");
	let external_config = fixture.root.join("worktree-list-config-real");
	std::fs::rename(&super_config, &external_config).unwrap();
	symlink("../../worktree-list-config-real", &super_config).unwrap();
	let super_transition = symlinked_deinit_transition(&super_config, &external_config, |config| {
		config.remove_subsection("submodule", "one");
	});
	let super_publication =
		prepare_displaced_deinit_config_for_test(&external_config, "super-list-symlink", |config| {
			config.remove_subsection("submodule", "one");
		});
	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	let mut intent: serde_json::Value =
		serde_json::from_slice(&std::fs::read(control.join("intent.json")).unwrap()).unwrap();
	intent["phase"] = serde_json::json!("super_config_prepared");
	intent["module_publication"] = module_publication;
	intent["super_transition"] = super_transition;
	intent["super_publication"] = super_publication;
	std::fs::write(
		control.join("intent.json"),
		serde_json::to_vec(&intent).unwrap(),
	)
	.unwrap();
	assert!(super_config.is_symlink());
	assert!(!external_config.exists());

	let listed = gta(
		&fixture.consumer,
		false,
		&["worktree", "list", "--porcelain"],
	);
	assert_success(
		&listed,
		"worktree list after restoring symlinked config target",
	);
	assert!(external_config.is_file());
	assert!(super_config.is_symlink());
	assert!(
		control.is_dir(),
		"worktree list must not advance the deinit intent"
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "resume after worktree list restored config");
	assert!(!control.exists());
	assert!(retired.join("file.txt").is_file());
}

#[cfg(unix)]
#[test]
fn ordinary_command_in_a_sibling_worktree_restores_the_owning_deinit_config() {
	let fixture = Fixture::new("deinit-sibling-command-setup");
	let owner = fixture.root.join("owner-worktree");
	let sibling = fixture.root.join("sibling-worktree");
	git_ok(
		&fixture.consumer,
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"deinit-owner",
			owner.to_str().unwrap(),
		],
	);
	git_ok(
		&fixture.consumer,
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"deinit-sibling",
			sibling.to_str().unwrap(),
		],
	);
	assert_success(
		&gta(&owner, true, &["submodule", "update", "--init"]),
		"initial linked-worktree update",
	);

	let module_git_dir = git_path(&owner, "modules/one");
	let parent = owner.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.sibling-command-setup";
	let retired_name = ".gitana-submodule-deinit-retired.sibling-command-setup";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-sibling-command-setup-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4_at(
		&owner,
		&fixture.old,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);
	let module_publication =
		publish_deinit_config_for_test(&module_git_dir.join("config"), "sibling-module", |config| {
			config.unset("core", None, "worktree");
		});
	let super_config = git_path(&owner, "config");
	let super_publication =
		prepare_displaced_deinit_config_for_test(&super_config, "sibling-super", |config| {
			config.remove_subsection("submodule", "one");
		});
	let control = git_path(&owner, "gitana-submodule-deinit");
	let mut intent: serde_json::Value =
		serde_json::from_slice(&std::fs::read(control.join("intent.json")).unwrap()).unwrap();
	intent["phase"] = serde_json::json!("super_config_prepared");
	intent["module_publication"] = module_publication;
	intent["super_publication"] = super_publication;
	std::fs::write(
		control.join("intent.json"),
		serde_json::to_vec(&intent).unwrap(),
	)
	.unwrap();
	assert!(!super_config.exists());

	let status = gta(&sibling, false, &["status"]);
	assert_success(
		&status,
		"sibling status after restoring the owning setup config",
	);
	assert!(super_config.is_file());
	assert!(
		control.is_dir(),
		"status must not advance the owning intent"
	);

	let recovered = gta(&owner, false, &["submodule", "deinit", "modules/one"]);
	assert_success(&recovered, "resume the owning linked-worktree deinit");
	assert!(!control.exists());
	assert!(retired.join("file.txt").is_file());
}

#[cfg(unix)]
#[test]
fn ordinary_command_in_a_bare_common_repository_restores_a_linked_deinit_config() {
	let fixture = Fixture::new("deinit-bare-command-setup");
	let bare = fixture.root.join("bare-consumer.git");
	let owner = fixture.root.join("bare-owner-worktree");
	git_ok(
		&fixture.root,
		&[
			"clone",
			"-q",
			"--bare",
			fixture.consumer.to_str().unwrap(),
			bare.to_str().unwrap(),
		],
	);
	git_ok(
		&bare,
		&[
			"worktree",
			"add",
			"-q",
			"-b",
			"deinit-owner",
			owner.to_str().unwrap(),
		],
	);
	assert_success(
		&gta(&owner, true, &["submodule", "update", "--init"]),
		"initial bare-linked worktree update",
	);

	let module_git_dir = git_path(&owner, "modules/one");
	let parent = owner.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.bare-command-setup";
	let retired_name = ".gitana-submodule-deinit-retired.bare-command-setup";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-bare-command-setup-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4_at(
		&owner,
		&fixture.old,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);
	let module_publication =
		publish_deinit_config_for_test(&module_git_dir.join("config"), "bare-module", |config| {
			config.unset("core", None, "worktree");
		});
	let super_config = bare.join("config");
	let super_publication =
		prepare_displaced_deinit_config_for_test(&super_config, "bare-super", |config| {
			config.remove_subsection("submodule", "one");
		});
	let control = git_path(&owner, "gitana-submodule-deinit");
	let mut intent: serde_json::Value =
		serde_json::from_slice(&std::fs::read(control.join("intent.json")).unwrap()).unwrap();
	intent["phase"] = serde_json::json!("super_config_prepared");
	intent["module_publication"] = module_publication;
	intent["super_publication"] = super_publication;
	std::fs::write(
		control.join("intent.json"),
		serde_json::to_vec(&intent).unwrap(),
	)
	.unwrap();
	assert!(!super_config.exists());

	let rev_parse = gta(&bare, false, &["rev-parse", "HEAD"]);
	assert_success(
		&rev_parse,
		"ordinary bare command after restoring the linked-worktree config",
	);
	assert!(super_config.is_file());
	assert!(
		control.is_dir(),
		"bootstrap must not advance the owning intent"
	);

	let recovered = gta(&owner, false, &["submodule", "deinit", "modules/one"]);
	assert_success(&recovered, "resume the bare-linked worktree deinit");
	assert!(!control.exists());
	assert!(retired.join("file.txt").is_file());
}

#[cfg(unix)]
#[test]
fn deinit_recovery_rejects_a_same_config_module_repository_replacement() {
	let fixture = Fixture::new("deinit-module-replacement");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let module_config = std::fs::read(module_git_dir.join("config")).unwrap();
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.module-replacement";
	let retired_name = ".gitana-submodule-deinit-retired.module-replacement";
	let prepared = parent.join(prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-module-replacement-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"detached",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);

	let original_module = module_git_dir.with_file_name("one-original");
	std::fs::rename(&module_git_dir, &original_module).unwrap();
	std::fs::create_dir(&module_git_dir).unwrap();
	std::fs::write(module_git_dir.join("config"), &module_config).unwrap();

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!recovered.status.success(),
		"a replacement repository must fail"
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("config")).unwrap(),
		module_config,
		"the foreign same-config repository is not edited"
	);
	assert!(
		prepared.join("file.txt").is_file(),
		"the checkout remains recoverable"
	);
	assert!(
		git_path(&fixture.consumer, "gitana-submodule-deinit").is_dir(),
		"the intent remains pending"
	);
}

#[cfg(unix)]
#[test]
fn deinit_recovery_rejects_a_new_worktree_local_attachment_override() {
	let fixture = Fixture::new("deinit-worktree-config-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let checkout = fixture.consumer.join("modules/one");
	git_ok(&checkout, &["config", "extensions.worktreeConfig", "true"]);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.worktree-config-recovery";
	let retired_name = ".gitana-submodule-deinit-retired.worktree-config-recovery";
	let prepared = parent.join(prepared_name);
	let retired = module_git_dir.join(retired_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-worktree-config-recovery-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::rename(&prepared, &retired).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"retired",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		Some("module_repository"),
	);
	let module_before = std::fs::read(module_git_dir.join("config")).unwrap();
	let super_before = std::fs::read(fixture.consumer.join(".git/config")).unwrap();
	std::fs::write(
		module_git_dir.join("config.worktree"),
		b"[core]\n\tworktree = ../../../foreign\n",
	)
	.unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"a new worktree-local attachment must fail recovery"
	);
	assert_eq!(
		std::fs::read(module_git_dir.join("config")).unwrap(),
		module_before,
		"the planned module config is not edited"
	);
	assert_eq!(
		std::fs::read(fixture.consumer.join(".git/config")).unwrap(),
		super_before,
		"the superproject registration is not edited"
	);
	assert!(retired.join("file.txt").is_file());
	let control = git_path(&fixture.consumer, "gitana-submodule-deinit");
	assert!(control.is_dir(), "the recovery intent remains pending");

	std::fs::remove_file(module_git_dir.join("config.worktree")).unwrap();
	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&recovered, "retry after removing the attachment override");
	assert!(!control.exists());
}

#[cfg(unix)]
#[test]
fn deinit_recovery_finishes_a_journaled_dirty_rollback() {
	let fixture = Fixture::new("deinit-rollback-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.rollback-recovery";
	let retired_name = ".gitana-submodule-deinit-retired.rollback-recovery";
	let prepared = parent.join(prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-rollback-recovery-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::write(prepared.join("file.txt"), b"dirty after proof\n").unwrap();
	write_deinit_intent_v4(
		&fixture,
		"rolling_back",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!recovered.status.success(),
		"the recovered dirty checkout is still refused"
	);
	assert_eq!(
		std::fs::read_to_string(target.join("file.txt")).unwrap(),
		"dirty after proof\n"
	);
	assert!(
		target.join(".git").is_file(),
		"the original mount is restored"
	);
	assert!(
		!git_path(&fixture.consumer, "gitana-submodule-deinit").exists(),
		"the rollback-complete journal is cleared"
	);
}

#[cfg(unix)]
#[test]
fn deinit_rollback_preserves_content_created_in_the_public_empty_directory() {
	let fixture = Fixture::new("deinit-rollback-public-content");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.rollback-public-content";
	let retired_name = ".gitana-submodule-deinit-retired.rollback-public-content";
	let prepared = parent.join(prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	let temporary = parent.join(".deinit-rollback-public-content-exchange");
	std::fs::rename(&target, &temporary).unwrap();
	std::fs::rename(&prepared, &target).unwrap();
	std::fs::rename(&temporary, &prepared).unwrap();
	std::fs::write(target.join("concurrent"), b"foreign\n").unwrap();
	write_deinit_intent_v4(
		&fixture,
		"rolling_back",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"rollback must not hide public content"
	);
	assert_eq!(
		std::fs::read_to_string(target.join("concurrent")).unwrap(),
		"foreign\n"
	);
	assert!(
		prepared.join(".git").is_file(),
		"the checkout remains displaced"
	);
	assert!(git_path(&fixture.consumer, "gitana-submodule-deinit").is_dir());

	std::fs::remove_file(target.join("concurrent")).unwrap();
	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!recovered.status.success(),
		"the original local-modification refusal is retained"
	);
	assert!(target.join(".git").is_file());
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());
}

#[cfg(unix)]
#[test]
fn deinit_recovery_clears_a_rollback_completed_before_intent_retirement() {
	let fixture = Fixture::new("deinit-rollback-completed");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.rollback-completed";
	let retired_name = ".gitana-submodule-deinit-retired.rollback-completed";
	let prepared = parent.join(prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	std::fs::write(target.join("file.txt"), b"dirty after proof\n").unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	std::fs::remove_dir(&prepared).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"rolled_back",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!recovered.status.success(),
		"the restored dirty checkout remains refused"
	);
	assert_eq!(
		std::fs::read_to_string(target.join("file.txt")).unwrap(),
		"dirty after proof\n"
	);
	assert!(
		!prepared.exists(),
		"recovery accepts a crash after exact rollback cleanup"
	);
	assert!(
		!git_path(&fixture.consumer, "gitana-submodule-deinit").exists(),
		"the completed rollback cannot wedge recovery"
	);
}

#[cfg(unix)]
#[test]
fn deinit_recovery_clears_a_cleaned_rollback_without_fresh_force() {
	let fixture = Fixture::new("deinit-rollback-cleaned");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let parent = fixture.consumer.join("modules");
	let target = parent.join("one");
	let prepared_name = ".gitana-submodule-deinit-prepared.rollback-cleaned";
	let retired_name = ".gitana-submodule-deinit-retired.rollback-cleaned";
	let prepared = parent.join(prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	std::fs::write(target.join("file.txt"), b"dirty after proof\n").unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	std::fs::remove_dir(&prepared).unwrap();
	write_deinit_intent_v4(
		&fixture,
		"rollback_cleaned",
		prepared_name,
		retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);

	let recovered = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert!(
		!recovered.status.success(),
		"the persisted dirty-checkout refusal remains visible"
	);
	assert!(stderr(&recovered).contains("local modifications"));
	assert_eq!(
		std::fs::read_to_string(target.join("file.txt")).unwrap(),
		"dirty after proof\n"
	);
	assert!(
		!git_path(&fixture.consumer, "gitana-submodule-deinit").exists(),
		"rollback-cleaned recovery must retire its journal without --force"
	);
}

#[cfg(unix)]
fn publish_deinit_config_for_test(
	path: &Path,
	purpose: &str,
	edit: impl FnOnce(&mut gitana_config::GitConfig),
) -> serde_json::Value {
	use std::os::unix::fs::MetadataExt as _;

	let mut config =
		gitana_config::GitConfig::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
	edit(&mut config);
	let prepared_name = format!(".gitana-config-prepared.{purpose}-recovery-test");
	let prepared = path.parent().unwrap().join(&prepared_name);
	std::fs::write(&prepared, config.render()).unwrap();
	let metadata = std::fs::symlink_metadata(&prepared).unwrap();
	std::fs::rename(&prepared, path).unwrap();
	serde_json::json!({
		"name": prepared_name,
		"device": metadata.dev(),
		"inode": metadata.ino()
	})
}

#[cfg(unix)]
fn prepare_displaced_deinit_config_for_test(
	path: &Path,
	purpose: &str,
	edit: impl FnOnce(&mut gitana_config::GitConfig),
) -> serde_json::Value {
	use std::os::unix::fs::MetadataExt as _;

	let mut config =
		gitana_config::GitConfig::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
	edit(&mut config);
	let prepared_name = format!(".gitana-config-prepared.{purpose}-recovery-test");
	let prepared = path.parent().unwrap().join(&prepared_name);
	let lock = path.with_file_name(format!(
		"{}.lock",
		path.file_name().unwrap().to_string_lossy()
	));
	std::fs::write(&prepared, config.render()).unwrap();
	let metadata = std::fs::symlink_metadata(&prepared).unwrap();
	std::fs::rename(&prepared, &lock).unwrap();
	std::fs::rename(path, &prepared).unwrap();
	serde_json::json!({
		"name": prepared_name,
		"device": metadata.dev(),
		"inode": metadata.ino()
	})
}

#[cfg(unix)]
fn deinit_transition(
	path: &Path,
	edit: impl FnOnce(&mut gitana_config::GitConfig),
) -> serde_json::Value {
	use std::os::unix::fs::MetadataExt as _;

	let text = std::fs::read_to_string(path).unwrap();
	let metadata = std::fs::metadata(path).unwrap();
	let parent_metadata = std::fs::metadata(path.parent().unwrap()).unwrap();
	let mut config = gitana_config::GitConfig::parse(&text).unwrap();
	let before_fingerprint = format!("{:x}", Sha256::digest(config.render().as_bytes()));
	edit(&mut config);
	let after_fingerprint = format!("{:x}", Sha256::digest(config.render().as_bytes()));
	serde_json::json!({
		"before_fingerprint": before_fingerprint,
		"after_fingerprint": after_fingerprint,
		"target": {
			"parent": {
				"device": parent_metadata.dev(),
				"inode": parent_metadata.ino()
			},
			"entry": {
				"state": "file",
				"device": metadata.dev(),
				"inode": metadata.ino()
			},
			"symlinks": []
		}
	})
}

#[cfg(unix)]
fn symlinked_deinit_transition(
	link: &Path,
	target: &Path,
	edit: impl FnOnce(&mut gitana_config::GitConfig),
) -> serde_json::Value {
	use std::os::unix::fs::MetadataExt as _;

	let text = std::fs::read_to_string(target).unwrap();
	let target_metadata = std::fs::symlink_metadata(target).unwrap();
	let parent_metadata = std::fs::metadata(target.parent().unwrap()).unwrap();
	let link_metadata = std::fs::symlink_metadata(link).unwrap();
	let mut config = gitana_config::GitConfig::parse(&text).unwrap();
	let before_fingerprint = format!("{:x}", Sha256::digest(config.render().as_bytes()));
	edit(&mut config);
	let after_fingerprint = format!("{:x}", Sha256::digest(config.render().as_bytes()));
	serde_json::json!({
		"before_fingerprint": before_fingerprint,
		"after_fingerprint": after_fingerprint,
		"target": {
			"parent": {
				"device": parent_metadata.dev(),
				"inode": parent_metadata.ino()
			},
			"entry": {
				"state": "file",
				"device": target_metadata.dev(),
				"inode": target_metadata.ino()
			},
			"symlinks": [{
				"device": link_metadata.dev(),
				"inode": link_metadata.ino()
			}]
		}
	})
}

#[cfg(unix)]
fn write_prepared_deinit_intent(fixture: &Fixture, suffix: &str) -> PathBuf {
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let target = fixture.consumer.join("modules/one");
	let prepared_name = format!(".gitana-submodule-deinit-prepared.{suffix}");
	let retired_name = format!(".gitana-submodule-deinit-retired.{suffix}");
	let prepared = fixture.consumer.join("modules").join(&prepared_name);
	std::fs::create_dir(&prepared).unwrap();
	let mount_metadata = std::fs::symlink_metadata(&target).unwrap();
	let prepared_metadata = std::fs::symlink_metadata(&prepared).unwrap();
	let module_metadata = std::fs::symlink_metadata(&module_git_dir).unwrap();
	write_deinit_intent_v4(
		fixture,
		"prepared",
		&prepared_name,
		&retired_name,
		&mount_metadata,
		&prepared_metadata,
		&module_metadata,
		None,
		None,
	);
	git_path(&fixture.consumer, "gitana-submodule-deinit")
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn write_deinit_intent_v4(
	fixture: &Fixture,
	phase: &str,
	prepared_name: &str,
	retired_name: &str,
	mount_metadata: &std::fs::Metadata,
	prepared_metadata: &std::fs::Metadata,
	module_metadata: &std::fs::Metadata,
	module_publication: Option<&Path>,
	retirement_location: Option<&str>,
) {
	write_deinit_intent_v4_at(
		&fixture.consumer,
		&fixture.old,
		phase,
		prepared_name,
		retired_name,
		mount_metadata,
		prepared_metadata,
		module_metadata,
		module_publication,
		retirement_location,
	);
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn write_deinit_intent_v4_at(
	repository: &Path,
	recorded: &str,
	phase: &str,
	prepared_name: &str,
	retired_name: &str,
	mount_metadata: &std::fs::Metadata,
	prepared_metadata: &std::fs::Metadata,
	module_metadata: &std::fs::Metadata,
	module_publication: Option<&Path>,
	retirement_location: Option<&str>,
) {
	use std::os::unix::fs::MetadataExt as _;

	let module_git_dir = git_path(repository, "modules/one");
	let module_config_path = module_git_dir.join("config");
	let super_config_path = git_path(repository, "config");
	let core_worktree = git(&module_git_dir, &["config", "--get", "core.worktree"])
		.trim()
		.to_owned();
	let module_transition = deinit_transition(&module_config_path, |config| {
		config.unset("core", None, "worktree");
	});
	let super_transition = deinit_transition(&super_config_path, |config| {
		config.remove_subsection("submodule", "one");
	});
	let module_publication = module_publication.map(|path| {
		let metadata = std::fs::symlink_metadata(path).unwrap();
		serde_json::json!({
			"name": path.file_name().unwrap().to_str().unwrap(),
			"device": metadata.dev(),
			"inode": metadata.ino()
		})
	});
	let control = git_path(repository, "gitana-submodule-deinit");
	std::fs::create_dir(&control).unwrap();
	std::fs::write(
		control.join("intent.json"),
		serde_json::to_vec(&serde_json::json!({
			"version": 4,
			"phase": phase,
			"name": "one",
			"path": "modules/one",
			"recorded": recorded,
			"force": false,
			"core_worktree": core_worktree,
			"module_transition": module_transition,
			"module_publication": module_publication,
			"super_transition": super_transition,
			"super_publication": null,
			"parent": "modules",
			"target": "one",
			"prepared": prepared_name,
			"displaced": prepared_name,
			"retired": retired_name,
			"rollback": null,
			"retirement_location": retirement_location,
			"mount_identity": {
				"device": mount_metadata.dev(),
				"inode": mount_metadata.ino()
			},
			"prepared_identity": {
				"device": prepared_metadata.dev(),
				"inode": prepared_metadata.ino()
			},
			"public_identity": {
				"device": prepared_metadata.dev(),
				"inode": prepared_metadata.ino()
			},
			"module_identity": {
				"device": module_metadata.dev(),
				"inode": module_metadata.ino()
			}
		}))
		.unwrap(),
	)
	.unwrap();
}

fn retired_checkout(module_git_dir: &Path) -> PathBuf {
	let retired: Vec<_> = std::fs::read_dir(module_git_dir)
		.unwrap()
		.map(|entry| entry.unwrap().path())
		.filter(|path| {
			path.file_name().is_some_and(|name| {
				name
					.to_string_lossy()
					.starts_with(".gitana-submodule-deinit-retired.")
			})
		})
		.collect();
	assert_eq!(retired.len(), 1, "expected exactly one retired checkout");
	retired.into_iter().next().unwrap()
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

	let initial = gta(
		&fixture.consumer,
		false,
		&["submodule", "update", "--init", "--depth", "1"],
	);
	assert_success(&initial, "initial rewritten HTTP submodule update");
	let module = fixture.consumer.join("modules/one");
	assert_shallow_one(&module);
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
fn local_clone_does_not_import_the_sources_remote_tracking_namespace() {
	let fixture = Fixture::new("clone-source-tracking-ref-conflict");
	let source_tip = git(&fixture.source, &["rev-parse", "HEAD"])
		.trim()
		.to_owned();
	let tree = git(&fixture.source, &["rev-parse", "HEAD^{tree}"])
		.trim()
		.to_owned();
	let tracking_only = git(
		&fixture.source,
		&[
			"-c",
			"user.name=Test",
			"-c",
			"user.email=test@example.com",
			"commit-tree",
			&tree,
			"-m",
			"tracking-only",
		],
	)
	.trim()
	.to_owned();
	git_ok(
		&fixture.source,
		&["update-ref", "refs/remotes/origin/main/child", &source_tip],
	);
	git_ok(
		&fixture.source,
		&["update-ref", "refs/remotes/upstream/hidden", &tracking_only],
	);
	let clone = fixture.root.join("tracking-ref-clone");

	let cloned = gta(
		&fixture.root,
		true,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "clone with a conflicting source tracking ref");
	assert_eq!(
		git(&clone, &["rev-parse", "refs/remotes/origin/main"]).trim(),
		source_tip
	);
	assert_eq!(
		git(&clone, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main"
	);
	let imported = Command::new("git")
		.arg("-C")
		.arg(&clone)
		.args([
			"show-ref",
			"--verify",
			"--quiet",
			"refs/remotes/origin/main/child",
		])
		.status()
		.expect("inspect imported tracking ref");
	assert!(
		!imported.success(),
		"the source repository's tracking refs must not be imported"
	);
	let tracking_commit = format!("{tracking_only}^{{commit}}");
	let tracking_object = Command::new("git")
		.arg("-C")
		.arg(&clone)
		.args(["cat-file", "-e", &tracking_commit])
		.output()
		.expect("inspect source-tracking-only object");
	assert!(
		!tracking_object.status.success(),
		"history reachable only from a filtered source tracking ref must not be downloaded"
	);
}

#[test]
fn fetch_reserves_remote_head_for_every_wildcard_mapping() {
	let fixture = Fixture::new("fetch-custom-wildcard-reserved-head");
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
			"remote.origin.fetch",
			"+refs/heads/team/*:refs/remotes/origin/*",
		],
	);
	git_ok(
		&fixture.source,
		&["update-ref", "refs/heads/team/HEAD", &fixture.old],
	);

	assert_success(
		&gta(&module, true, &["fetch"]),
		"fetch through a wildcard mapping onto remote HEAD",
	);
	assert_eq!(
		git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main",
		"the wildcard mapping must preserve a symbolic convenience ref"
	);

	git_ok(
		&module,
		&["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
	);
	git_ok(
		&module,
		&["update-ref", "refs/remotes/origin/HEAD", &fixture.old],
	);
	let advanced = fixture.commit_source("team head\n", "advance team HEAD");
	git_ok(
		&fixture.source,
		&["update-ref", "refs/heads/team/HEAD", &advanced],
	);
	assert_success(
		&gta(&module, true, &["fetch"]),
		"fetch while a direct remote HEAD occupies the reserved destination",
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/HEAD"]).trim(),
		fixture.old,
		"the wildcard mapping must preserve a direct convenience ref"
	);
}

#[test]
fn local_clone_reserves_origin_head_from_a_branch_named_head() {
	let fixture = Fixture::new("clone-literal-head-branch");
	git_ok(
		&fixture.source,
		&["update-ref", "refs/heads/HEAD", &fixture.old],
	);
	let clone = fixture.root.join("literal-head-clone");

	let cloned = gta(
		&fixture.root,
		true,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "clone with a literal HEAD branch");
	assert_eq!(
		git(&clone, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main",
		"the wildcard branch import must not overwrite the convenience ref"
	);
}

#[test]
fn local_clone_detaches_when_the_source_head_targets_a_filtered_tracking_ref() {
	let fixture = Fixture::new("clone-filtered-head-target");
	git_ok(
		&fixture.source,
		&["update-ref", "refs/remotes/upstream/main", &fixture.old],
	);
	git_ok(
		&fixture.source,
		&["symbolic-ref", "HEAD", "refs/remotes/upstream/main"],
	);
	let clone = fixture.root.join("filtered-head-clone");

	let cloned = gta(
		&fixture.root,
		true,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "clone whose source HEAD target is filtered");
	assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), fixture.old);
	let symbolic = Command::new("git")
		.arg("-C")
		.arg(&clone)
		.args(["symbolic-ref", "-q", "HEAD"])
		.output()
		.expect("inspect cloned HEAD");
	assert!(
		!symbolic.status.success(),
		"the cloned HEAD must be detached"
	);
	assert_eq!(
		std::fs::read_to_string(clone.join("file.txt")).unwrap(),
		"old\n"
	);
	let imported = Command::new("git")
		.arg("-C")
		.arg(&clone)
		.args([
			"show-ref",
			"--verify",
			"--quiet",
			"refs/remotes/upstream/main",
		])
		.status()
		.expect("inspect filtered source tracking ref");
	assert!(!imported.success());
}

#[test]
fn local_clone_omits_origin_head_when_a_tracking_descendant_blocks_it() {
	let fixture = Fixture::new("clone-origin-head-descendant");
	git_ok(
		&fixture.source,
		&["update-ref", "refs/heads/HEAD/foo", &fixture.old],
	);
	let clone = fixture.root.join("head-descendant-clone");

	let cloned = gta(
		&fixture.root,
		true,
		&[
			"clone",
			fixture.source.to_str().unwrap(),
			clone.to_str().unwrap(),
		],
	);
	assert_success(&cloned, "clone with an origin/HEAD descendant");
	assert_eq!(
		git(&clone, &["rev-parse", "refs/remotes/origin/HEAD/foo"]).trim(),
		fixture.old
	);
	let convenience = Command::new("git")
		.arg("-C")
		.arg(&clone)
		.args(["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
		.output()
		.expect("inspect origin HEAD convenience ref");
	assert!(
		!convenience.status.success(),
		"the unrepresentable convenience ref must be omitted"
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

	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--depth", "1"],
	);
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

	let update = gta_with_environment(
		&fixture.consumer,
		&["submodule", "update", "--depth", "1"],
		&environment,
	);
	assert_success(&update, "SSH exact-object fallback");
	assert_eq!(
		std::fs::read_to_string(&sessions).unwrap().lines().count(),
		2,
		"normal fetch and exact-object fallback must use separate SSH sessions"
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), hidden);
	assert_shallow_one(&module);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"hidden over ssh\n"
	);
}

#[test]
fn an_existing_current_module_still_fetches_advertised_refs() {
	let fixture = Fixture::new("existing-advertised-fetch");
	fixture.commit_source("first advertised\n", "first advertised commit");
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			&format!("file://{}", fixture.source.display()),
		],
	);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--init", "--depth", "1"],
		),
		"initial shallow update",
	);
	let module = fixture.consumer.join("modules/one");
	assert_shallow_one(&module);
	let next = fixture.commit_source("second advertised\n", "second advertised commit");

	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--depth", "1"],
	);
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
	assert_shallow_one(&module);
	assert_eq!(
		git(&module, &["rev-list", "--count", &next]).trim(),
		"1",
		"the separately fetched advertised tip must retain its shallow boundary"
	);
}

#[test]
fn initial_remote_update_records_a_detached_head_for_no_fetch() {
	let fixture = Fixture::new("initial-remote-detached-head");
	git_ok(&fixture.source, &["checkout", "--detach", "-q"]);
	let detached = fixture.commit_source("initial detached\n", "initial detached HEAD");

	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "--remote"],
	);
	assert_success(&update, "initial update from a detached remote HEAD");
	let module = fixture.consumer.join("modules/one");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), detached);
	let symbolic = Command::new("git")
		.arg("-C")
		.arg(&module)
		.args(["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
		.output()
		.expect("inspect initial remote HEAD");
	assert!(
		!symbolic.status.success(),
		"a detached initial selection must publish a direct remote HEAD"
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/HEAD"]).trim(),
		detached
	);

	git_ok(&module, &["checkout", "--detach", "-q", &fixture.old]);
	git_ok(
		&fixture.consumer,
		&["config", "--unset-all", "submodule.one.url"],
	);
	git_ok(&module, &["config", "--unset-all", "remote.origin.url"]);
	let local = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote", "--no-fetch"],
	);
	assert_success(&local, "reuse the initial direct remote HEAD");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), detached);
}

#[test]
fn initial_remote_update_preserves_a_case_aliased_tracking_branch() {
	let fixture = Fixture::new("initial-remote-case-aliased-head");
	if !directory_is_case_insensitive(&fixture.consumer) {
		return;
	}
	git_ok(
		&fixture.source,
		&["update-ref", "refs/heads/head", &fixture.old],
	);
	let default_tip = fixture.commit_source("new default\n", "advance default branch");

	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--init", "--remote"],
		),
		"initial remote update with a case-aliased tracking branch",
	);
	let module = fixture.consumer.join("modules/one");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), default_tip);
	let symbolic = Command::new("git")
		.arg("-C")
		.arg(&module)
		.args(["symbolic-ref", "-q", "refs/remotes/origin/head"])
		.output()
		.expect("inspect case-aliased tracking branch");
	assert!(
		!symbolic.status.success(),
		"the tracking branch must remain direct rather than becoming the convenience symref"
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/head"]).trim(),
		fixture.old
	);
	let no_fetch = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote", "--no-fetch"],
	);
	assert!(
		!no_fetch.status.success(),
		"a case-aliased branch must not satisfy the exact remote HEAD lookup"
	);
	assert!(
		stderr(&no_fetch).contains("refs/remotes/origin/HEAD"),
		"unexpected missing-target error: {}",
		stderr(&no_fetch)
	);
	assert_eq!(
		git(&module, &["rev-parse", "HEAD"]).trim(),
		default_tip,
		"the rejected alias must not move module HEAD"
	);

	assert_success(
		&gta(&module, true, &["fetch"]),
		"fetch with a case-aliased tracking branch",
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/head"]).trim(),
		fixture.old
	);
}

#[test]
fn fetch_rejects_a_tracking_branch_aliased_to_remote_head_by_case() {
	let fixture = Fixture::new("fetch-case-aliased-remote-head");
	if !directory_is_case_insensitive(&fixture.consumer) {
		return;
	}
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--init", "--remote"],
		),
		"initial remote update",
	);
	let module = fixture.consumer.join("modules/one");
	assert_eq!(
		git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main"
	);

	let aliased_tip = fixture.commit_source("case alias\n", "create case-aliased branch tip");
	git_ok(
		&fixture.source,
		&["update-ref", "refs/heads/head", &aliased_tip],
	);
	git_ok(&fixture.source, &["reset", "--hard", &fixture.old]);

	let fetch = gta(&module, true, &["fetch"]);
	assert!(
		!fetch.status.success(),
		"a differently spelled tracking destination must not update through origin/HEAD"
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/main"]).trim(),
		fixture.old,
		"the aliased branch must not move the remote HEAD terminal"
	);
	assert_eq!(
		git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main"
	);
}

#[test]
fn initial_remote_update_rejects_a_branch_named_head_before_repository_creation() {
	let fixture = Fixture::new("initial-remote-literal-head");
	git_ok(
		&fixture.source,
		&["update-ref", "refs/heads/HEAD", &fixture.old],
	);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.branch",
			"HEAD",
		],
	);

	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "--remote"],
	);
	assert!(
		!update.status.success(),
		"reserved branch unexpectedly succeeded"
	);
	assert!(
		stderr(&update).contains("reserved remote HEAD ref"),
		"unexpected error: {}",
		stderr(&update)
	);
	assert!(
		!git_path(&fixture.consumer, "modules/one").exists(),
		"target validation must precede module repository creation"
	);
	assert!(
		!git_path(&fixture.consumer, "gitana-submodule-update").exists(),
		"target validation must precede intent publication"
	);
}

#[test]
fn remote_update_selects_the_branch_tip_and_no_fetch_uses_local_tracking_state() {
	let fixture = Fixture::new("remote-submodule-update");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let first = fixture.commit_source("remote first\n", "advance remote once");
	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote"],
	);
	assert_success(&update, "remote branch update");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), first);
	assert_eq!(
		git(&fixture.consumer, &["rev-parse", "HEAD:modules/one"]).trim(),
		fixture.old,
		"remote selection must not change the superproject gitlink"
	);

	let second = fixture.commit_source("remote second\n", "advance remote twice");
	let stale = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote", "--no-fetch"],
	);
	assert_success(&stale, "no-fetch remote update from stale tracking state");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), first);

	git_allow(&module, &["fetch", "origin"]);
	git_ok(
		&fixture.consumer,
		&["config", "--unset-all", "submodule.one.url"],
	);
	git_ok(&module, &["config", "--unset-all", "remote.origin.url"]);
	let local = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote", "--no-fetch"],
	);
	assert_success(&local, "URL-free no-fetch remote update");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), second);

	let recorded = gta(&fixture.consumer, true, &["submodule", "update", "-N"]);
	assert_success(&recorded, "URL-free no-fetch gitlink update");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
}

#[test]
fn remote_update_repairs_remote_head_for_a_later_no_fetch_update() {
	let fixture = Fixture::new("remote-submodule-repair-head");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(
		&module,
		&["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
	);
	let current = fixture.commit_source("remote head repaired\n", "advance remote head");

	let fetched = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote"],
	);
	assert_success(&fetched, "remote update repairs its HEAD symref");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), current);
	assert_eq!(
		git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main"
	);

	git_ok(
		&fixture.consumer,
		&["config", "--unset-all", "submodule.one.url"],
	);
	git_ok(&module, &["config", "--unset-all", "remote.origin.url"]);
	let local = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote", "--no-fetch"],
	);
	assert_success(&local, "use the repaired remote HEAD without transport");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), current);
}

#[test]
fn no_fetch_remote_branch_uses_the_configured_tracking_destination() {
	let fixture = Fixture::new("remote-submodule-custom-tracking");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.branch",
			"main",
		],
	);
	git_ok(
		&module,
		&[
			"config",
			"--replace-all",
			"remote.origin.fetch",
			"+refs/heads/main:refs/custom/upstream-main",
		],
	);
	let current = fixture.commit_source("custom tracking\n", "advance custom tracking");

	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"remote update through a custom tracking destination",
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/custom/upstream-main"]).trim(),
		current
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/main"]).trim(),
		fixture.old,
		"the conventional tracking ref remains deliberately stale"
	);

	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote", "--no-fetch"],
		),
		"no-fetch update through a custom tracking destination",
	);
	assert_eq!(
		git(&module, &["rev-parse", "HEAD"]).trim(),
		current,
		"no-fetch must not select the stale conventional ref"
	);
}

#[test]
fn no_fetch_remote_branches_require_exact_ref_spelling() {
	let fixture = Fixture::new("no-fetch-exact-remote-branch");
	if !directory_is_case_insensitive(&fixture.consumer) {
		return;
	}
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(
		&module,
		&["update-ref", "refs/remotes/origin/feature", &fixture.old],
	);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.branch",
			"Feature",
		],
	);

	let named = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote", "--no-fetch"],
	);
	assert!(
		!named.status.success(),
		"a case alias must not satisfy a named remote branch"
	);
	assert!(
		stderr(&named).contains("refs/remotes/origin/Feature"),
		"unexpected named-remote error: {}",
		stderr(&named)
	);

	git_ok(&module, &["checkout", "-q", "-b", "holder"]);
	git_ok(&module, &["update-ref", "refs/heads/feature", &fixture.old]);
	git_ok(&module, &["config", "branch.holder.remote", "."]);
	let local = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote", "--no-fetch"],
	);
	assert!(
		!local.status.success(),
		"a case alias must not satisfy a local dot-remote branch"
	);
	assert!(
		stderr(&local).contains("refs/heads/Feature"),
		"unexpected dot-remote error: {}",
		stderr(&local)
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
}

#[test]
fn remote_update_rejects_the_reserved_remote_head_destination() {
	let fixture = Fixture::new("remote-submodule-reserved-head");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let default_tip = fixture.commit_source("new default\n", "advance default branch");
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"update the default branch and remote HEAD",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), default_tip);
	assert_eq!(
		git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main"
	);
	git_ok(
		&fixture.source,
		&["update-ref", "refs/heads/HEAD", &fixture.old],
	);
	let fetched = gta(&module, true, &["fetch"]);
	assert_success(
		&fetched,
		"ordinary fetch with a literal HEAD branch and a convenience ref",
	);
	assert_eq!(
		git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main",
		"wildcard fetch must preserve the remote HEAD convenience ref"
	);
	git_ok(
		&fixture.source,
		&["symbolic-ref", "HEAD", "refs/heads/HEAD"],
	);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"select an advertised remote HEAD whose target occupies the reserved mapping",
	);
	let symbolic = Command::new("git")
		.arg("-C")
		.arg(&module)
		.args(["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
		.output()
		.expect("inspect direct remote HEAD");
	assert!(
		!symbolic.status.success(),
		"an ambiguous advertised target must be recorded directly"
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/HEAD"]).trim(),
		fixture.old
	);
	git_ok(
		&fixture.source,
		&["symbolic-ref", "HEAD", "refs/heads/main"],
	);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"restore the ordinary remote HEAD convenience ref",
	);
	assert_eq!(
		git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/remotes/origin/main"
	);
	git_ok(&module, &["checkout", "--detach", "-q", &fixture.old]);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.branch",
			"HEAD",
		],
	);

	for arguments in [
		&["submodule", "update", "--remote", "--no-fetch"][..],
		&["submodule", "update", "--remote"][..],
	] {
		let update = gta(&fixture.consumer, true, arguments);
		assert!(
			!update.status.success(),
			"remote update unexpectedly accepted the reserved destination: stdout={} stderr={}",
			String::from_utf8_lossy(&update.stdout),
			String::from_utf8_lossy(&update.stderr)
		);
		assert!(
			String::from_utf8_lossy(&update.stderr).contains("reserved remote HEAD ref"),
			"remote update did not explain the reserved destination: {}",
			String::from_utf8_lossy(&update.stderr)
		);
		assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
		assert_eq!(
			git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
			"refs/remotes/origin/main"
		);
		assert_eq!(
			git(&module, &["rev-parse", "refs/remotes/origin/main"]).trim(),
			default_tip
		);
	}

	git_ok(
		&module,
		&[
			"config",
			"--add",
			"remote.origin.fetch",
			"+refs/heads/HEAD:refs/custom/branch-head",
		],
	);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"fetch a literal HEAD branch through a custom destination",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
	assert_eq!(
		git(&module, &["rev-parse", "refs/custom/branch-head"]).trim(),
		fixture.old
	);

	git_ok(&module, &["checkout", "--detach", "-q", &default_tip]);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote", "--no-fetch"],
		),
		"reuse a literal HEAD branch from its custom destination",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);

	git_ok(
		&fixture.source,
		&["symbolic-ref", "HEAD", "refs/heads/HEAD"],
	);
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"--unset-all",
			"submodule.one.branch",
		],
	);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"repair remote HEAD through the later exact destination",
	);
	assert_eq!(
		git(&module, &["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
		"refs/custom/branch-head"
	);

	git_ok(&module, &["checkout", "--detach", "-q", &default_tip]);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote", "--no-fetch"],
		),
		"reuse the repaired remote HEAD through the exact destination",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
}

#[test]
fn remote_update_refuses_a_branch_not_selected_by_fetch_refspecs() {
	let fixture = Fixture::new("remote-submodule-unselected-branch");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.branch",
			"main",
		],
	);
	let current = fixture.commit_source("unselected branch\n", "advance unselected branch");

	for (refspec, description) in [
		("^refs/heads/main", "excluded branch"),
		(
			"+refs/heads/topic:refs/remotes/origin/topic",
			"unmapped branch",
		),
	] {
		git_ok(
			&module,
			&["config", "--replace-all", "remote.origin.fetch", refspec],
		);
		let update = gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		);
		assert!(
			!update.status.success(),
			"remote update unexpectedly accepted {description}: stdout={} stderr={}",
			String::from_utf8_lossy(&update.stdout),
			String::from_utf8_lossy(&update.stderr)
		);
		assert_eq!(
			git(&module, &["rev-parse", "HEAD"]).trim(),
			fixture.old,
			"a rejected {description} must not move the module"
		);
		assert_eq!(
			git(&module, &["rev-parse", "refs/remotes/origin/main"]).trim(),
			fixture.old,
			"a rejected {description} must not move its tracking ref"
		);
	}
	assert_ne!(current, fixture.old);
}

#[test]
fn remote_update_records_a_detached_advertised_head_for_no_fetch() {
	let fixture = Fixture::new("remote-submodule-detached-head");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(&fixture.source, &["checkout", "--detach", "-q"]);
	let detached = fixture.commit_source("detached remote\n", "detached remote HEAD");

	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"remote update from a detached advertised HEAD",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), detached);
	let symbolic = Command::new("git")
		.arg("-C")
		.arg(&module)
		.args(["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
		.output()
		.expect("inspect detached remote HEAD");
	assert!(
		!symbolic.status.success(),
		"a detached advertisement must publish a direct remote HEAD"
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/HEAD"]).trim(),
		detached
	);

	git_ok(&module, &["checkout", "--detach", "-q", &fixture.old]);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote", "--no-fetch"],
		),
		"no-fetch update from the direct remote HEAD",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), detached);
}

#[test]
fn remote_update_preserves_unrepresentable_remote_head_occupants() {
	let fixture = Fixture::new("remote-submodule-blocked-head");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let remote_head = git_path(&fixture.consumer, "modules/one").join("refs/remotes/origin/HEAD");
	git_ok(
		&module,
		&["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
	);
	std::fs::create_dir(&remote_head).unwrap();
	let first = fixture.commit_source("blocked head one\n", "advance past empty directory");

	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"remote update with an empty remote HEAD directory",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), first);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/main"]).trim(),
		first
	);
	assert!(
		remote_head.is_dir(),
		"the optional ref conflict is preserved"
	);

	std::fs::remove_dir(&remote_head).unwrap();
	std::fs::write(&remote_head, b"ref: bad ref\n").unwrap();
	let second = fixture.commit_source("blocked head two\n", "advance past malformed occupant");
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"remote update with a malformed symbolic remote HEAD occupant",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), second);
	assert_eq!(std::fs::read(&remote_head).unwrap(), b"ref: bad ref\n");
}

#[test]
fn remote_update_accepts_a_remote_name_ending_in_a_dot() {
	let fixture = Fixture::new("remote-name-trailing-dot");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(&module, &["switch", "-c", "topic"]);
	git_ok(
		&module,
		&["remote", "add", "backup.", fixture.source.to_str().unwrap()],
	);
	git_ok(&module, &["config", "branch.topic.remote", "backup."]);
	let current = fixture.commit_source("trailing dot remote\n", "advance trailing dot remote");

	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote"],
	);
	assert_success(&update, "remote update through backup.");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), current);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/backup./main"]).trim(),
		current
	);
}

#[test]
fn remote_update_rejects_dot_prefixed_branch_components_before_checkout() {
	let fixture = Fixture::new("remote-branch-dot-component");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");

	for branch in [".topic", "foo/.topic"] {
		git_ok(
			&fixture.consumer,
			&[
				"config",
				"-f",
				".gitmodules",
				"submodule.one.branch",
				branch,
			],
		);
		let update = gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		);
		assert!(
			!update.status.success(),
			"remote update unexpectedly accepted {branch}: stdout={} stderr={}",
			stdout(&update),
			stderr(&update)
		);
		assert!(stderr(&update).contains("invalid submodule branch"));
		assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
	}
}

#[test]
fn remote_update_rejects_a_non_fast_forward_before_checkout() {
	let fixture = Fixture::new("remote-submodule-rejected-rewind");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	let advanced = fixture.commit_source("advanced\n", "advance remote");
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"advance module from the remote branch",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), advanced);

	git_ok(
		&module,
		&[
			"config",
			"--replace-all",
			"remote.origin.fetch",
			"refs/heads/*:refs/remotes/origin/*",
		],
	);
	git_ok(&fixture.source, &["reset", "--hard", &fixture.old]);
	let rejected = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote"],
	);
	assert!(
		!rejected.status.success(),
		"a rejected tracking-ref rewind must fail the update"
	);
	assert!(
		stderr(&rejected).contains("some remote-tracking refs were not updated (non-fast-forward)"),
		"unexpected rejection error: {}",
		stderr(&rejected)
	);
	assert_eq!(
		git(&module, &["rev-parse", "HEAD"]).trim(),
		advanced,
		"a rejected remote target must not be checked out"
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/main"]).trim(),
		advanced,
		"the rejected tracking ref must retain its old value"
	);
	assert_eq!(
		std::fs::read_to_string(module.join("file.txt")).unwrap(),
		"advanced\n"
	);
}

#[test]
fn remote_update_honors_branch_configuration_and_dot_requires_a_branch() {
	let fixture = Fixture::new("remote-submodule-branch");
	let dev = fixture.commit_source("development\n", "development tip");
	git_ok(&fixture.source, &["branch", "dev", &dev]);
	git_ok(&fixture.source, &["reset", "--hard", &fixture.old]);
	git_ok(
		&fixture.consumer,
		&["config", "-f", ".gitmodules", "submodule.one.branch", "dev"],
	);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--init", "--remote"],
		),
		"configured remote branch update",
	);
	let module = fixture.consumer.join("modules/one");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), dev);
	git_ok(
		&fixture.consumer,
		&["config", "submodule.one.branch", "main"],
	);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"superproject-local branch override",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);

	git_ok(&fixture.consumer, &["config", "submodule.one.branch", "."]);
	git_ok(&fixture.consumer, &["checkout", "--detach", "-q"]);
	let detached = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--remote"],
	);
	assert!(!detached.status.success());
	assert!(
		stderr(&detached).contains("superproject HEAD is detached"),
		"unexpected detached-branch error: {}",
		stderr(&detached)
	);
}

#[test]
fn remote_update_uses_the_current_module_branch_remote_and_supports_dot() {
	let fixture = Fixture::new("remote-submodule-selected-remote");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(&module, &["checkout", "-q", "-b", "selected"]);
	git_ok(
		&module,
		&["remote", "add", "backup", fixture.source.to_str().unwrap()],
	);
	git_ok(&module, &["config", "branch.selected.remote", "backup"]);
	let remote_tip = fixture.commit_source("backup remote\n", "advance backup");
	git_ok(&module, &["config", "remote.origin.url", "missing-origin"]);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"named module remote update",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), remote_tip);

	git_ok(&module, &["checkout", "-q", "-B", "local"]);
	std::fs::write(module.join("file.txt"), b"module-local\n").unwrap();
	git_ok(&module, &["add", "file.txt"]);
	commit(&module, "module-local target");
	let local_tip = git(&module, &["rev-parse", "HEAD"]).trim().to_owned();
	git_ok(&module, &["config", "branch.local.remote", "."]);
	git_ok(
		&fixture.consumer,
		&["config", "submodule.one.branch", "local"],
	);
	git_ok(
		&fixture.consumer,
		&["config", "--unset-all", "submodule.one.url"],
	);
	git_ok(&module, &["config", "--unset-all", "remote.origin.url"]);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"dot module remote update",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), local_tip);

	git_ok(&module, &["config", "branch.local.remote", "backup"]);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"missing registration still skips a named module remote",
	);
	assert_eq!(
		git(&module, &["rev-parse", "HEAD"]).trim(),
		local_tip,
		"the named remote must remain skipped without registration"
	);
}

#[test]
fn recursive_clone_remote_submodules_selects_remote_tips_and_last_flag_wins() {
	let fixture = Fixture::new("clone-remote-submodules");
	let tip = fixture.commit_source("remote clone tip\n", "advance before clone");
	for (name, flags, expected) in [
		("remote", vec!["--remote-submodules"], tip.as_str()),
		(
			"remote-last",
			vec!["--no-remote-submodules", "--remote-submodules"],
			tip.as_str(),
		),
		(
			"recorded-last",
			vec!["--remote-submodules", "--no-remote-submodules"],
			fixture.old.as_str(),
		),
	] {
		let destination = fixture.root.join(name);
		let mut arguments = vec!["clone", "--recurse-submodules"];
		arguments.extend(flags);
		arguments.push(fixture.superproject.to_str().unwrap());
		arguments.push(destination.to_str().unwrap());
		let clone = gta(&fixture.root, true, &arguments);
		assert_success(&clone, "recursive clone remote-submodules policy");
		assert_eq!(
			git(&destination.join("modules/one"), &["rev-parse", "HEAD"]).trim(),
			expected
		);
	}
}

#[test]
fn recursive_remote_update_propagates_target_and_fetch_policy() {
	let root = unique_tmp("recursive-remote-update");
	let leaf = root.join("leaf");
	let parent = root.join("parent");
	let superproject = root.join("super");
	for repository in [&leaf, &parent, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(repository.join("file.txt"), b"recorded\n").unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "recorded");
	}
	git_allow(&parent, &["submodule", "add", "../leaf", "child"]);
	commit(&parent, "add child");
	git_allow(
		&superproject,
		&["submodule", "add", "../parent", "modules/parent"],
	);
	commit(&superproject, "add parent");
	std::fs::write(leaf.join("file.txt"), b"leaf tip\n").unwrap();
	git_ok(&leaf, &["add", "file.txt"]);
	commit(&leaf, "advance leaf");
	let leaf_tip = git(&leaf, &["rev-parse", "HEAD"]).trim().to_owned();
	std::fs::write(parent.join("file.txt"), b"parent tip\n").unwrap();
	git_ok(&parent, &["add", "file.txt"]);
	commit(&parent, "advance parent");
	let parent_tip = git(&parent, &["rev-parse", "HEAD"]).trim().to_owned();
	let consumer = root.join("consumer");
	assert_success(
		&gta(
			&root,
			false,
			&[
				"clone",
				superproject.to_str().unwrap(),
				consumer.to_str().unwrap(),
			],
		),
		"clone root",
	);
	assert_success(
		&gta(
			&consumer,
			true,
			&["submodule", "update", "--init", "--recursive", "--remote"],
		),
		"recursive remote update",
	);
	assert_eq!(
		git(&consumer.join("modules/parent"), &["rev-parse", "HEAD"]).trim(),
		parent_tip
	);
	assert_eq!(
		git(
			&consumer.join("modules/parent/child"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		leaf_tip
	);
	std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recursive_remote_recovery_does_not_rerun_the_recovered_owner() {
	let root = unique_tmp("recursive-remote-recovery");
	let leaf = root.join("leaf");
	let parent = root.join("parent");
	let superproject = root.join("super");
	for repository in [&leaf, &parent, &superproject] {
		std::fs::create_dir_all(repository).unwrap();
		init_repository(repository, None);
		std::fs::write(repository.join("file.txt"), b"recorded\n").unwrap();
		git_ok(repository, &["add", "file.txt"]);
		commit(repository, "recorded");
	}
	git_allow(&parent, &["submodule", "add", "../leaf", "child"]);
	commit(&parent, "add child");
	let recorded_parent = git(&parent, &["rev-parse", "HEAD"]).trim().to_owned();
	git_allow(
		&superproject,
		&["submodule", "add", "../parent", "modules/parent"],
	);
	commit(&superproject, "add parent");
	let consumer = root.join("consumer");
	assert_success(
		&gta(
			&root,
			false,
			&[
				"clone",
				superproject.to_str().unwrap(),
				consumer.to_str().unwrap(),
			],
		),
		"clone root",
	);
	assert_success(
		&gta(
			&consumer,
			true,
			&["submodule", "update", "--init", "--recursive"],
		),
		"initialize nested modules",
	);

	std::fs::write(parent.join("file.txt"), b"pinned parent\n").unwrap();
	git_ok(&parent, &["add", "file.txt"]);
	commit(&parent, "pinned parent");
	let pinned_parent = git(&parent, &["rev-parse", "HEAD"]).trim().to_owned();
	assert_success(
		&gta(
			&consumer,
			true,
			&["submodule", "update", "--remote", "modules/parent"],
		),
		"fetch the pinned parent",
	);
	let mounted_parent = consumer.join("modules/parent");
	let module_origin = git(&mounted_parent, &["config", "--get", "remote.origin.url"])
		.trim()
		.to_owned();
	assert_success(
		&gta(
			&consumer,
			false,
			&["submodule", "deinit", "-f", "modules/parent"],
		),
		"retain the parent before recovery",
	);
	assert_success(
		&gta(&consumer, false, &["submodule", "init", "modules/parent"]),
		"restore the registration owned by the interrupted update",
	);
	let control = write_v5_recovery_intent(
		&consumer,
		"modules/parent",
		"modules/parent",
		&recorded_parent,
		&pinned_parent,
		&module_origin,
		"module",
		Some("origin"),
		true,
	);

	std::fs::write(parent.join("file.txt"), b"later parent\n").unwrap();
	git_ok(&parent, &["add", "file.txt"]);
	commit(&parent, "later parent");
	let later_parent = git(&parent, &["rev-parse", "HEAD"]).trim().to_owned();
	std::fs::write(leaf.join("file.txt"), b"later leaf\n").unwrap();
	git_ok(&leaf, &["add", "file.txt"]);
	commit(&leaf, "later leaf");
	let later_leaf = git(&leaf, &["rev-parse", "HEAD"]).trim().to_owned();

	let recovered = gta(
		&consumer,
		true,
		&["submodule", "update", "--init", "--recursive", "--remote"],
	);
	assert_success(&recovered, "recover once and continue recursive descent");
	assert_eq!(
		stdout(&recovered)
			.matches("Submodule path 'modules/parent':")
			.count(),
		1,
		"the recovered owner must not be checked out again in the normal pass"
	);
	assert_eq!(
		git(&mounted_parent, &["rev-parse", "HEAD"]).trim(),
		pinned_parent
	);
	assert_ne!(
		git(&mounted_parent, &["rev-parse", "HEAD"]).trim(),
		later_parent
	);
	assert_eq!(
		git(&mounted_parent.join("child"), &["rev-parse", "HEAD"]).trim(),
		later_leaf,
		"a recovered owner must still contribute its recursive descendants"
	);
	assert!(!control.exists());

	assert_success(
		&gta(
			&consumer,
			true,
			&["submodule", "update", "--remote", "modules/parent"],
		),
		"advance the parent in a later invocation",
	);
	assert_eq!(
		git(&mounted_parent, &["rev-parse", "HEAD"]).trim(),
		later_parent
	);
	std::fs::remove_dir_all(root).unwrap();
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
fn update_init_reuses_the_superproject_lock_for_a_self_source() {
	let fixture = Fixture::new("self-source-lock-reuse");
	let recorded = git(&fixture.consumer, &["rev-parse", "HEAD"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			fixture.consumer.to_str().unwrap(),
		],
	);
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{recorded},modules/one"),
		],
	);

	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "modules/one"],
	);
	assert_success(&update, "update --init from the locked superproject itself");
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		recorded
	);
}

#[test]
fn existing_update_reuses_the_module_lock_for_a_self_origin() {
	let fixture = Fixture::new("self-origin-lock-reuse");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let module = fixture.consumer.join("modules/one");
	git_ok(
		&module,
		&["config", "remote.origin.url", module.to_str().unwrap()],
	);

	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "modules/one"],
	);
	assert_success(&update, "update from the locked module repository itself");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), fixture.old);
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
fn abandoned_legacy_recovery_uses_the_current_gitlink() {
	let fixture = Fixture::new("abandoned-legacy-current-gitlink");
	assert_success(
		&gta(&fixture.consumer, false, &["submodule", "init"]),
		"register module",
	);
	let source_url = git(&fixture.consumer, &["config", "--get", "submodule.one.url"])
		.trim()
		.to_owned();
	let current = fixture.commit_source("current gitlink\n", "current gitlink");
	git_ok(
		&fixture.consumer,
		&[
			"update-index",
			"--cacheinfo",
			&format!("160000,{current},modules/one"),
		],
	);
	let control = write_v2_recovery_intent(&fixture, "one", "modules/one", &fixture.old, &source_url);

	let update = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&update, "restart after abandoning an empty legacy intent");
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		current
	);
	assert!(!control.exists());
}

#[test]
fn abandoned_legacy_recovery_reselects_the_current_remote_head() {
	let fixture = Fixture::new("abandoned-legacy-current-remote-head");
	assert_success(
		&gta(&fixture.consumer, false, &["submodule", "init"]),
		"register module",
	);
	let source_url = git(&fixture.consumer, &["config", "--get", "submodule.one.url"])
		.trim()
		.to_owned();
	let control = write_v4_recovery_intent(
		&fixture,
		"one",
		"modules/one",
		&fixture.old,
		&source_url,
		"superproject",
	);
	let current = fixture.commit_source("current remote head\n", "current remote head");

	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "--remote"],
	);
	assert_success(
		&update,
		"reselect remote HEAD after abandoning legacy intent",
	);
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		current
	);
	assert!(!control.exists());
}

#[cfg(any(unix, windows))]
#[test]
fn module_scoped_recovery_locks_config_before_source_validation() {
	let fixture = Fixture::new("module-recovery-config-locked");
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
	let module_directory = Dir::open_ambient_dir(&module_git_dir, ambient_authority()).unwrap();
	let mutation =
		acquire_submodule_config_mutation_lease(&module_directory, &module_git_dir).unwrap();
	let config = module_git_dir.join("config");
	let displaced = module_git_dir.join("config.recovery-displaced");
	std::fs::rename(&config, &displaced).unwrap();

	let refused = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert!(!refused.status.success(), "a busy module config must fail");
	assert!(
		stderr(&refused).contains("submodule update is already running"),
		"recovery read the displaced config before taking its guard: {}",
		stderr(&refused),
	);
	assert!(control.join("intent.json").is_file());

	std::fs::rename(&displaced, &config).unwrap();
	drop(mutation);
	let recovered = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&recovered, "retry module-scoped recovery");
	assert!(!control.exists());
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
	git_ok(&fixture.consumer, &["config", "submodule.one.branch", "."]);
	git_ok(&fixture.consumer, &["checkout", "--detach", "-q"]);

	let recovered = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "--remote"],
	);
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
fn v5_remote_recovery_finishes_the_recorded_target_before_advancing_again() {
	let fixture = Fixture::new("remote-v5-recovery");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let first = fixture.commit_source("first remote target\n", "first remote target");
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"fetch first remote target",
	);
	let module = fixture.consumer.join("modules/one");
	let module_origin = git(&module, &["config", "--get", "remote.origin.url"])
		.trim()
		.to_owned();
	git_ok(
		&fixture.consumer,
		&["submodule", "deinit", "-f", "modules/one"],
	);
	let control = write_v5_recovery_intent(
		&fixture.consumer,
		"one",
		"modules/one",
		&fixture.old,
		&first,
		&module_origin,
		"module",
		Some("origin"),
		true,
	);
	let second = fixture.commit_source("second remote target\n", "second remote target");
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--init", "--remote"],
		),
		"resume exact remote target",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), first);
	assert!(!control.exists());
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"advance after exact recovery",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), second);
}

#[test]
fn unpublished_v5_remote_recovery_reprepares_the_exact_selected_target() {
	let fixture = Fixture::new("remote-v5-unpublished-recovery");
	assert_success(
		&gta(&fixture.consumer, false, &["submodule", "init"]),
		"register module",
	);
	let first = fixture.commit_source("first selected target\n", "first selected target");
	let source = git(&fixture.consumer, &["config", "--get", "submodule.one.url"])
		.trim()
		.to_owned();
	let control = write_v5_recovery_intent(
		&fixture.consumer,
		"one",
		"modules/one",
		&fixture.old,
		&first,
		&source,
		"superproject",
		None,
		true,
	);
	let second = fixture.commit_source("second selected target\n", "second selected target");
	git_ok(&fixture.consumer, &["config", "submodule.one.branch", "."]);
	git_ok(&fixture.consumer, &["checkout", "--detach", "-q"]);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote"],
		),
		"reprepare exact selected target",
	);
	let module = fixture.consumer.join("modules/one");
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), first);
	assert_ne!(git(&module, &["rev-parse", "HEAD"]).trim(), second);
	let symbolic = Command::new("git")
		.arg("-C")
		.arg(&module)
		.args(["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
		.output()
		.expect("inspect recovered remote HEAD");
	assert!(
		!symbolic.status.success(),
		"v5 recovery must retain the original remote-HEAD publication policy"
	);
	assert_eq!(
		git(&module, &["rev-parse", "refs/remotes/origin/HEAD"]).trim(),
		first
	);
	assert!(!control.exists());

	git_ok(
		&fixture.consumer,
		&["config", "--unset-all", "submodule.one.branch"],
	);
	git_ok(
		&fixture.consumer,
		&["config", "--unset-all", "submodule.one.url"],
	);
	git_ok(&module, &["config", "--unset-all", "remote.origin.url"]);
	assert_success(
		&gta(
			&fixture.consumer,
			true,
			&["submodule", "update", "--remote", "--no-fetch"],
		),
		"reuse the recovered direct remote HEAD",
	);
	assert_eq!(git(&module, &["rev-parse", "HEAD"]).trim(), first);
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
	git_ok(&fixture.consumer, &["config", "submodule.one.branch", "."]);
	git_ok(&fixture.consumer, &["checkout", "--detach", "-q"]);

	let recovered = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "--remote"],
	);
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
	fixture.commit_source("new\n", "advance SHA-256 source");
	git_ok(
		&fixture.consumer,
		&[
			"config",
			"-f",
			".gitmodules",
			"submodule.one.url",
			&format!("file://{}", fixture.source.display()),
		],
	);
	let update = gta(
		&fixture.consumer,
		true,
		&["submodule", "update", "--init", "--depth", "1"],
	);
	assert_success(&update, "shallow SHA-256 update");
	assert_eq!(
		git(
			&fixture.consumer.join("modules/one"),
			&["rev-parse", "HEAD"]
		)
		.trim(),
		fixture.old
	);
	assert_shallow_one(&fixture.consumer.join("modules/one"));
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
	let git_dir = initialized.consumer.join(".git");
	let update_lock = git_dir.join("gitana-submodule-update.lock");
	let config_lock = git_dir.join("gitana-submodule-config.lock");
	let _ = std::fs::remove_file(&update_lock);
	let _ = std::fs::remove_file(&config_lock);
	let before = std::fs::read(&config).unwrap();
	let before_metadata = std::fs::metadata(&config).unwrap();
	std::fs::write(git_dir.join("config.lock"), b"held").unwrap();
	#[cfg(unix)]
	let original_permissions = {
		use std::os::unix::fs::PermissionsExt as _;
		let permissions = std::fs::metadata(&git_dir).unwrap().permissions();
		std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
		permissions
	};

	let repeated = gta(&initialized.consumer, false, &["submodule", "init"]);
	#[cfg(unix)]
	std::fs::set_permissions(&git_dir, original_permissions).unwrap();
	assert_success(&repeated, "already-initialized no-op init");
	assert_eq!(std::fs::read(&config).unwrap(), before);
	assert!(!update_lock.exists(), "no-op init created its update lock");
	assert!(!config_lock.exists(), "no-op init created its config lock");
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
	let empty_git_dir = empty.consumer.join(".git");
	let empty_update_lock = empty_git_dir.join("gitana-submodule-update.lock");
	let empty_config_lock = empty_git_dir.join("gitana-submodule-config.lock");
	let _ = std::fs::remove_file(&empty_update_lock);
	let _ = std::fs::remove_file(&empty_config_lock);
	let empty_before = std::fs::read(&empty_config).unwrap();
	std::fs::write(empty_git_dir.join("config.lock"), b"held").unwrap();
	#[cfg(unix)]
	let empty_original_permissions = {
		use std::os::unix::fs::PermissionsExt as _;
		let permissions = std::fs::metadata(&empty_git_dir).unwrap().permissions();
		std::fs::set_permissions(&empty_git_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
		permissions
	};
	let no_gitlinks = gta(&empty.consumer, false, &["submodule", "init"]);
	#[cfg(unix)]
	std::fs::set_permissions(&empty_git_dir, empty_original_permissions).unwrap();
	assert_success(&no_gitlinks, "no-gitlink no-op init");
	assert_eq!(std::fs::read(&empty_config).unwrap(), empty_before);
	assert!(
		!empty_update_lock.exists(),
		"empty-selection init created its update lock"
	);
	assert!(
		!empty_config_lock.exists(),
		"empty-selection init created its config lock"
	);
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

	let deinit = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "modules/one"],
	);
	assert_success(&deinit, "deinit through symlinked module config");
	assert!(
		std::fs::symlink_metadata(&config)
			.unwrap()
			.file_type()
			.is_symlink(),
		"deinit must preserve the module config symlink"
	);
	assert!(
		!std::fs::read_to_string(&target)
			.unwrap()
			.lines()
			.any(|line| line.trim_start().starts_with("worktree =")),
		"deinit must remove core.worktree from the symlink target"
	);
	assert_eq!(
		std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
		0o600
	);

	let reattach = gta(&fixture.consumer, true, &["submodule", "update", "--init"]);
	assert_success(&reattach, "reattach through symlinked module config");
	assert!(
		std::fs::symlink_metadata(&config)
			.unwrap()
			.file_type()
			.is_symlink(),
		"reattachment must preserve the module config symlink"
	);
	assert!(fixture.consumer.join("modules/one/file.txt").is_file());
}

#[cfg(unix)]
#[test]
fn deinit_rejects_a_module_config_target_inside_the_selected_checkout_before_mutation() {
	let fixture = Fixture::new("checkout-contained-module-config");
	assert_success(
		&gta(&fixture.consumer, true, &["submodule", "update", "--init"]),
		"initial update",
	);
	let checkout = fixture.consumer.join("modules/one");
	let module_git_dir = git_path(&fixture.consumer, "modules/one");
	let config = module_git_dir.join("config");
	let contained_target = checkout.join("config-real");
	std::fs::rename(&config, &contained_target).unwrap();
	std::os::unix::fs::symlink("../../../modules/one/config-real", &config).unwrap();

	let refused = gta(
		&fixture.consumer,
		false,
		&["submodule", "deinit", "--force", "modules/one"],
	);
	assert!(
		!refused.status.success(),
		"contained config target must fail"
	);
	assert!(
		stderr(&refused).contains("is inside the selected checkout"),
		"unexpected diagnostic: {}",
		stderr(&refused)
	);
	assert!(checkout.join("file.txt").is_file());
	assert!(checkout.join(".git").is_file());
	assert!(
		std::fs::read_to_string(&contained_target)
			.unwrap()
			.lines()
			.any(|line| line.trim_start().starts_with("worktree =")),
		"preflight failure must preserve the module attachment"
	);
	assert!(
		std::fs::read_to_string(fixture.consumer.join(".git/config"))
			.unwrap()
			.contains("[submodule \"one\"]"),
		"preflight failure must preserve registration"
	);
	assert!(!git_path(&fixture.consumer, "gitana-submodule-deinit").exists());

	let external_target = fixture.root.join("external-module-config");
	std::fs::remove_file(&config).unwrap();
	std::fs::rename(&contained_target, &external_target).unwrap();
	std::os::unix::fs::symlink(&external_target, &config).unwrap();
	assert_success(
		&gta(
			&fixture.consumer,
			false,
			&["submodule", "deinit", "--force", "modules/one"],
		),
		"deinit through external module config symlink",
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
fn an_update_lock_failure_precedes_initialization() {
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
		!stderr(&update).contains("Submodule 'one'"),
		"initialization must not start before acquiring the shared mutation lock: {}",
		stderr(&update)
	);
	assert!(stderr(&update).contains("already running"));
	assert!(
		!std::fs::read_to_string(fixture.consumer.join(".git/config"))
			.unwrap()
			.contains("active = true"),
		"the rejected update must not initialize the module"
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

#[test]
fn set_branch_edits_an_exact_root_mapping_without_a_gitlink_or_staging() {
	let root = unique_tmp("set-branch-exact");
	init_repository(&root, None);
	let modules = root.join(".gitmodules");
	std::fs::write(
		&modules,
		"# preserved\n[submodule \"one\"]\n\tpath = modules/one\n\turl = ../source\n\tbranch = old # keep\n",
	)
	.unwrap();
	let nested = root.join("nested");
	std::fs::create_dir(&nested).unwrap();

	let set = gta(
		&nested,
		false,
		&[
			"submodule",
			"set-branch",
			"-b",
			"bad..name",
			"--",
			"modules/one",
		],
	);
	assert_success(&set, "set branch from a subdirectory");
	assert!(stdout(&set).is_empty());
	assert!(stderr(&set).is_empty());
	let text = std::fs::read_to_string(&modules).unwrap();
	assert!(text.starts_with("# preserved\n"));
	assert!(text.contains("branch = bad..name # keep"));
	assert!(git(&root, &["diff", "--cached", "--name-only"]).is_empty());

	let before_wrong_path = std::fs::read(&modules).unwrap();
	let wrong_path = gta(
		&nested,
		false,
		&["submodule", "set-branch", "--branch=main", "./modules/one"],
	);
	assert!(!wrong_path.status.success());
	assert_eq!(std::fs::read(&modules).unwrap(), before_wrong_path);

	let empty = gta(
		&root,
		false,
		&["submodule", "set-branch", "--branch=", "modules/one"],
	);
	assert_success(&empty, "record an empty branch verbatim");
	let configured = Command::new("git")
		.args(["config", "-f"])
		.arg(&modules)
		.args(["--get", "submodule.one.branch"])
		.output()
		.unwrap();
	assert!(configured.status.success());
	assert_eq!(configured.stdout, b"\n");

	let clear = gta(
		&root,
		false,
		&["submodule", "set-branch", "-d", "modules/one"],
	);
	assert_success(&clear, "clear branch");
	let before_noop = std::fs::read(&modules).unwrap();
	#[cfg(unix)]
	let before_noop_inode = {
		use std::os::unix::fs::MetadataExt as _;
		std::fs::metadata(&modules).unwrap().ino()
	};
	let already_default = gta(
		&root,
		false,
		&["submodule", "set-branch", "--default", "modules/one"],
	);
	assert_eq!(already_default.status.code(), Some(1));
	assert!(stdout(&already_default).is_empty());
	assert!(stderr(&already_default).is_empty());
	assert_eq!(std::fs::read(&modules).unwrap(), before_noop);
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt as _;
		assert_eq!(
			std::fs::metadata(&modules).unwrap().ino(),
			before_noop_inode
		);
	}

	std::fs::write(
		&modules,
		"[submodule \"one\"]\n\tpath = modules/one\n\tbranch = main\n\tbranch = next\n",
	)
	.unwrap();
	let before_multiple = std::fs::read(&modules).unwrap();
	let multiple = gta(
		&root,
		false,
		&["submodule", "set-branch", "--branch=other", "modules/one"],
	);
	assert!(!multiple.status.success());
	assert_eq!(std::fs::read(&modules).unwrap(), before_multiple);
	std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn set_branch_preserves_a_symlinked_gitmodules_file() {
	use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

	let root = unique_tmp("set-branch-symlink");
	init_repository(&root, None);
	let metadata = root.join("metadata");
	std::fs::create_dir(&metadata).unwrap();
	let target = metadata.join("modules-config");
	std::fs::write(
		&target,
		"[submodule \"one\"]\n\tpath = modules/one\n\turl = ../source\n",
	)
	.unwrap();
	std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
	symlink("metadata/modules-config", root.join(".gitmodules")).unwrap();

	let set = gta(
		&root,
		false,
		&["submodule", "set-branch", "--branch", ".", "modules/one"],
	);
	assert_success(&set, "set branch through symlinked .gitmodules");
	assert!(
		std::fs::symlink_metadata(root.join(".gitmodules"))
			.unwrap()
			.file_type()
			.is_symlink()
	);
	assert_eq!(
		std::fs::read_link(root.join(".gitmodules")).unwrap(),
		PathBuf::from("metadata/modules-config")
	);
	assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
	assert!(
		std::fs::read_to_string(target)
			.unwrap()
			.contains("branch = .")
	);
	std::fs::remove_dir_all(root).unwrap();
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

fn configure_module_shallow(repository: &Path, name: &str, source: &Path, shallow: &str) {
	let url_key = format!("submodule.{name}.url");
	let source = format!("file://{}", source.display());
	git_ok(
		repository,
		&["config", "-f", ".gitmodules", &url_key, &source],
	);
	set_module_shallow(repository, name, shallow);
}

fn set_module_shallow(repository: &Path, name: &str, shallow: &str) {
	let key = format!("submodule.{name}.shallow");
	git_ok(repository, &["config", "-f", ".gitmodules", &key, shallow]);
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
	write_v4_recovery_intent_at(
		&fixture.consumer,
		name,
		path,
		recorded,
		resolved_source,
		source_context,
	)
}

fn write_v4_recovery_intent_at(
	repository: &Path,
	name: &str,
	path: &str,
	recorded: &str,
	resolved_source: &str,
	source_context: &str,
) -> PathBuf {
	let control = git_path(repository, "gitana-submodule-update");
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

#[allow(clippy::too_many_arguments)]
fn write_v5_recovery_intent(
	repository: &Path,
	name: &str,
	path: &str,
	gitlink: &str,
	target: &str,
	resolved_source: &str,
	source_context: &str,
	remote: Option<&str>,
	record_remote_head: bool,
) -> PathBuf {
	let control = git_path(repository, "gitana-submodule-update");
	std::fs::create_dir_all(&control).unwrap();
	let fingerprint = Sha256::digest(gitana_remote::redact_password(resolved_source).as_bytes());
	let fingerprint = fingerprint
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>();
	let remote = remote.map_or_else(String::new, |remote| format!(",\"remote\":\"{remote}\""));
	std::fs::write(
		control.join("intent.json"),
		format!(
			"{{\"version\":5,\"name\":\"{name}\",\"path\":\"{path}\",\"gitlink\":\"{gitlink}\",\"target\":\"{target}\",\"source_fingerprint\":\"{fingerprint}\",\"source_context\":\"{source_context}\",\"record_remote_head\":{record_remote_head}{remote}}}"
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

fn assert_shallow_one(repository: &Path) {
	assert_eq!(
		git(repository, &["rev-parse", "--is-shallow-repository"]).trim(),
		"true",
		"{} must be shallow",
		repository.display()
	);
	assert_eq!(
		git(repository, &["rev-list", "--count", "HEAD"]).trim(),
		"1",
		"{} must retain only the checked-out commit's shallow history",
		repository.display()
	);
}

fn assert_not_shallow(repository: &Path) {
	assert_eq!(
		git(repository, &["rev-parse", "--is-shallow-repository"]).trim(),
		"false",
		"{} must retain complete history",
		repository.display()
	);
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

fn directory_is_case_insensitive(directory: &Path) -> bool {
	let probe = directory.join(".gitana-case-probe");
	let _ = std::fs::remove_dir_all(&probe);
	std::fs::create_dir(&probe).unwrap();
	std::fs::write(probe.join("CaseProbe"), b"probe").unwrap();
	let insensitive = probe.join("caseprobe").exists();
	let _ = std::fs::remove_dir_all(probe);
	insensitive
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
