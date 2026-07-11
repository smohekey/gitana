//! `gta config` end-to-end: read/write the local `.git/config`, cross-checked against real git
//! (git reads what gta writes and vice versa).

use std::path::PathBuf;
use std::process::Command;

fn init(tag: &str) -> PathBuf {
	let work = unique_tmp(tag);
	gta(work.to_str().unwrap(), &["init"], b"");
	work
}

#[test]
fn config_set_get_and_git_interop() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = init("gta-config-set");
	let w = work.to_str().unwrap();

	gta(w, &["config", "user.name", "A U Thor"], b"");
	assert_eq!(gta(w, &["config", "user.name"], b""), "A U Thor\n");
	assert_eq!(gta(w, &["config", "--get", "user.name"], b""), "A U Thor\n");
	// git reads what gta wrote.
	assert_eq!(git(w, &["config", "user.name"]).trim(), "A U Thor");

	// ...and gta reads what git wrote.
	git(w, &["config", "user.email", "a@example.com"]);
	assert_eq!(gta(w, &["config", "user.email"], b""), "a@example.com\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_subsection_round_trips() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-sub");
	let w = work.to_str().unwrap();

	gta(w, &["config", "remote.origin.url", "http://example/x"], b"");
	assert_eq!(
		gta(w, &["config", "remote.origin.url"], b""),
		"http://example/x\n"
	);
	assert_eq!(
		git(w, &["config", "remote.origin.url"]).trim(),
		"http://example/x"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_add_and_get_all() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-multi");
	let w = work.to_str().unwrap();

	gta(w, &["config", "--add", "remote.origin.fetch", "one"], b"");
	gta(w, &["config", "--add", "remote.origin.fetch", "two"], b"");
	assert_eq!(
		gta(w, &["config", "--get-all", "remote.origin.fetch"], b""),
		"one\ntwo\n"
	);
	// git sees both values too.
	assert_eq!(
		git(w, &["config", "--get-all", "remote.origin.fetch"]),
		"one\ntwo\n"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_unset_removes_the_key() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-unset");
	let w = work.to_str().unwrap();

	gta(w, &["config", "user.name", "A U Thor"], b"");
	gta(w, &["config", "--unset", "user.name"], b"");
	// The key is gone from the local config: a `--local` lookup exits non-zero, and git agrees. Both
	// scope the read to the local file so an unrelated global `user.name` (the machine's own, or one
	// git's default merged read would surface) cannot mask the removal.
	gta_fail(w, &["config", "--local", "user.name"]);
	assert!(
		!Command::new("git")
			.args(["-C", w, "config", "--local", "user.name"])
			.output()
			.unwrap()
			.status
			.success()
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_bool_and_int() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-typed");
	let w = work.to_str().unwrap();

	gta(w, &["config", "core.flag", "yes"], b"");
	assert_eq!(gta(w, &["config", "--bool", "core.flag"], b""), "true\n");
	gta(w, &["config", "pack.size", "2k"], b"");
	assert_eq!(gta(w, &["config", "--int", "pack.size"], b""), "2048\n");
	assert_eq!(git(w, &["config", "--int", "pack.size"]).trim(), "2048");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_list_includes_set_values() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-list");
	let w = work.to_str().unwrap();

	gta(w, &["config", "user.name", "A U Thor"], b"");
	let list = gta(w, &["config", "--list"], b"");
	assert!(
		list.lines().any(|l| l == "user.name=A U Thor"),
		"list: {list}"
	);
	assert!(
		list.lines().any(|l| l == "extensions.objectformat=sha256"),
		"list: {list}"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_unset_rejects_a_value_pattern() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-unset-value");
	let w = work.to_str().unwrap();

	gta(w, &["config", "user.name", "Alice"], b"");
	// `--unset key value` is a value-pattern op in git; we reject it rather than deleting blindly,
	// so the key is preserved.
	gta_fail(w, &["config", "--unset", "user.name", "Bob"]);
	assert_eq!(gta(w, &["config", "user.name"], b""), "Alice\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_bare_variable_is_present_with_empty_value() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-bare");
	let w = work.to_str().unwrap();

	// Append a valueless variable directly (git itself always writes `= value`).
	let cfg = work.join(".git/config");
	let mut text = std::fs::read_to_string(&cfg).unwrap();
	text.push_str("[core]\n\tbareflag\n");
	std::fs::write(&cfg, text).unwrap();

	// Present but valueless: an empty line, exit 0 — matching git, and distinct from a missing key.
	assert_eq!(gta(w, &["config", "core.bareflag"], b""), "\n");
	assert_eq!(gta(w, &["config", "--get-all", "core.bareflag"], b""), "\n");
	assert_eq!(git(w, &["config", "core.bareflag"]), "\n");
	// A bare variable reads as boolean-true.
	assert_eq!(
		gta(w, &["config", "--bool", "core.bareflag"], b""),
		"true\n"
	);
	// ...but reading it as an integer is a parse error (present, not absent), like git.
	let err = gta_fail(w, &["config", "--int", "core.bareflag"]);
	assert!(err.contains("not an integer"), "stderr: {err}");
	assert!(
		!Command::new("git")
			.args(["-C", w, "config", "--int", "core.bareflag"])
			.output()
			.unwrap()
			.status
			.success(),
		"git also rejects --int of a bare variable"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_missing_key_exits_nonzero() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-missing");
	let w = work.to_str().unwrap();

	gta_fail(w, &["config", "no.such.key"]);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_writes_preserve_comments_and_layout() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-preserve");
	let w = work.to_str().unwrap();

	// Seed a config with comments, a blank line, and an inline note around an existing value.
	let cfg = work.join(".git/config");
	let mut text = std::fs::read_to_string(&cfg).unwrap();
	text.push_str("\n# a kept comment\n[user]\n\tname = Old   # inline note\n");
	std::fs::write(&cfg, &text).unwrap();

	// Change the value, add a sibling, and unset an unrelated key.
	gta(w, &["config", "user.name", "New Name"], b"");
	gta(w, &["config", "user.email", "a@example.com"], b"");

	let after = std::fs::read_to_string(&cfg).unwrap();
	assert!(
		after.contains("# a kept comment"),
		"comment dropped:\n{after}"
	);
	assert!(
		after.contains("# inline note"),
		"inline note dropped:\n{after}"
	);
	// The set edited only the value in place.
	assert!(after.contains("name = New Name   # inline note"), "{after}");
	// git agrees on the resulting values.
	assert_eq!(git(w, &["config", "user.name"]).trim(), "New Name");
	assert_eq!(git(w, &["config", "user.email"]).trim(), "a@example.com");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_set_into_file_without_final_newline() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-nonewline");
	let w = work.to_str().unwrap();

	// A config whose last line has no trailing newline.
	let cfg = work.join(".git/config");
	let mut text = std::fs::read_to_string(&cfg).unwrap();
	if text.ends_with('\n') {
		text.pop();
	}
	std::fs::write(&cfg, &text).unwrap();

	gta(w, &["config", "user.name", "Alice"], b"");

	let after = std::fs::read_to_string(&cfg).unwrap();
	// The new key must be on its own line, never glued onto the previous one.
	assert!(
		!after
			.lines()
			.any(|l| l.contains("\tname") && l.matches('=').count() > 1),
		"keys glued onto one line:\n{after}"
	);
	// git reads the result cleanly.
	assert_eq!(git(w, &["config", "user.name"]).trim(), "Alice");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn config_set_refuses_to_collapse_multi_valued_key() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-multi-set");
	let w = work.to_str().unwrap();

	gta(w, &["config", "--add", "remote.origin.fetch", "one"], b"");
	gta(w, &["config", "--add", "remote.origin.fetch", "two"], b"");

	// A plain set over a multi-valued key is refused, leaving both values intact (as git does).
	let err = gta_fail(w, &["config", "remote.origin.fetch", "three"]);
	assert!(err.contains("multiple values"), "stderr: {err}");
	assert_eq!(
		gta(w, &["config", "--get-all", "remote.origin.fetch"], b""),
		"one\ntwo\n"
	);

	// --replace-all collapses the multiple values into one (as git does).
	gta(
		w,
		&["config", "--replace-all", "remote.origin.fetch", "three"],
		b"",
	);
	assert_eq!(
		gta(w, &["config", "--get-all", "remote.origin.fetch"], b""),
		"three\n"
	);
	assert_eq!(
		git(w, &["config", "--get-all", "remote.origin.fetch"]),
		"three\n"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn identity_resolves_from_global_config() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-identity-global");
	let w = work.to_str().unwrap();
	let gdir = unique_tmp("gta-identity-globalcfg");
	let global = gdir.join("gitconfig");
	std::fs::write(
		&global,
		"[user]\n\tname = Global User\n\temail = global@example.com\n",
	)
	.unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	// Commit with NO GIT_* identity env: the identity must come from the global config, not git's
	// `unknown <unknown@localhost>` placeholder.
	std::fs::write(work.join("a.txt"), b"hi\n").unwrap();
	ok_stdout(gta_env(w, &["add", "a.txt"], &env));
	ok_stdout(gta_env(w, &["commit", "-m", "c"], &env));

	let body = ok_stdout(gta_env(w, &["cat-file", "-p", "HEAD"], &env));
	let author = body.lines().find(|l| l.starts_with("author ")).unwrap();
	let committer = body.lines().find(|l| l.starts_with("committer ")).unwrap();
	assert!(
		author.contains("Global User <global@example.com>"),
		"author: {author}"
	);
	assert!(
		committer.contains("Global User <global@example.com>"),
		"committer: {committer}"
	);

	// Oracle: stock git, pointed at the same global config, resolves the same identity.
	assert_eq!(
		ok_stdout(git_env(w, &["config", "user.name"], &env)).trim(),
		"Global User"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn identity_precedence_local_over_global() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-identity-prec");
	let w = work.to_str().unwrap();
	let gdir = unique_tmp("gta-prec-globalcfg");
	let global = gdir.join("gitconfig");
	std::fs::write(&global, "[user]\n\tname = Global\n").unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	// A local value overrides the global one; gta and git resolve the same precedence.
	ok_stdout(gta_env(w, &["config", "user.name", "Local"], &env));
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "user.name"], &env)).trim(),
		"Local"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "user.name"], &env)).trim(),
		"Local"
	);
	// Scoped reads restrict to one file: `--global` still sees the global value, `--local` only local.
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--global", "user.name"], &env)).trim(),
		"Global"
	);
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--local", "user.name"], &env)).trim(),
		"Local"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn global_config_disabled_by_dev_null() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-identity-devnull");
	let w = work.to_str().unwrap();
	let gdir = unique_tmp("gta-devnull-globalcfg");
	let global = gdir.join("gitconfig");
	std::fs::write(&global, "[user]\n\temail = g@x\n").unwrap();

	// With the global active the email resolves; pointing the global at /dev/null disables it, so the
	// key is unset — matching stock git, which honours GIT_CONFIG_GLOBAL the same way.
	let active = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "user.email"], &active)).trim(),
		"g@x"
	);
	let disabled = [
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	assert!(
		!gta_env(w, &["config", "user.email"], &disabled)
			.status
			.success()
	);
	assert!(
		!git_env(w, &["config", "user.email"], &disabled)
			.status
			.success()
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn config_global_write_interops_with_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-globalwrite");
	let w = work.to_str().unwrap();
	let gdir = unique_tmp("gta-gwrite-globalcfg");
	let global = gdir.join("gitconfig");
	std::fs::write(&global, "").unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	// gta writes to the global file; git reads it back.
	ok_stdout(gta_env(
		w,
		&["config", "--global", "user.name", "Via Gta"],
		&env,
	));
	assert_eq!(
		ok_stdout(git_env(w, &["config", "--global", "user.name"], &env)).trim(),
		"Via Gta"
	);
	// ...and git writes to it; gta reads it back.
	ok_stdout(git_env(
		w,
		&["config", "--global", "user.email", "via@git"],
		&env,
	));
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--global", "user.email"], &env)).trim(),
		"via@git"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn malformed_lower_config_is_an_error() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-badglobal");
	let w = work.to_str().unwrap();
	let gdir = unique_tmp("gta-badglobal-cfg");
	let global = gdir.join("gitconfig");
	// An unterminated section header: git aborts on it, and so must gta rather than silently ignoring
	// the file and reading a lower-precedence (or empty) value.
	std::fs::write(&global, "[user\n\tname = x\n").unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	assert!(
		!gta_env(w, &["config", "user.email"], &env).status.success(),
		"gta must abort on a malformed global config"
	);
	assert!(
		!git_env(w, &["config", "user.email"], &env).status.success(),
		"git aborts too (oracle)"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn system_scope_ignores_nosystem() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-sysscope");
	let w = work.to_str().unwrap();
	let sdir = unique_tmp("gta-sysscope-cfg");
	let system = sdir.join("gitconfig");
	std::fs::write(&system, "[user]\n\tname = SysUser\n").unwrap();
	// NOSYSTEM drops the system layer from a merged read, but an explicit --system still targets the
	// file — for both read and write — exactly as git does.
	let env = [
		("GIT_CONFIG_SYSTEM", system.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--system", "user.name"], &env)).trim(),
		"SysUser"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "--system", "user.name"], &env)).trim(),
		"SysUser"
	);
	// A --system write lands in the file (no panic on the suppressed-system path) and git reads it.
	ok_stdout(gta_env(
		w,
		&["config", "--system", "core.answer", "42"],
		&env,
	));
	assert_eq!(
		ok_stdout(git_env(w, &["config", "--system", "core.answer"], &env)).trim(),
		"42"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&sdir).ok();
}

#[test]
fn global_scope_reads_only_the_selected_file() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-globalselect");
	let w = work.to_str().unwrap();
	// A HOME with both a XDG config and a ~/.gitconfig, each carrying a distinct key.
	let home = unique_tmp("gta-globalselect-home");
	std::fs::create_dir_all(home.join(".config/git")).unwrap();
	std::fs::write(
		home.join(".config/git/config"),
		"[user]\n\txonly = fromxdg\n\tname = XdgName\n",
	)
	.unwrap();
	std::fs::write(home.join(".gitconfig"), "[user]\n\tname = HomeName\n").unwrap();
	let env = [
		("HOME", home.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	// `--global` selects the single file (`~/.gitconfig` when it exists) — XDG is NOT merged into it,
	// so an XDG-only key reads as unset. git resolves `--global` identically.
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--global", "user.name"], &env)).trim(),
		"HomeName"
	);
	assert!(
		!gta_env(w, &["config", "--global", "user.xonly"], &env)
			.status
			.success()
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "--global", "user.name"], &env)).trim(),
		"HomeName"
	);
	assert!(
		!git_env(w, &["config", "--global", "user.xonly"], &env)
			.status
			.success()
	);
	// ...but the unscoped merged read *does* layer XDG beneath ~/.gitconfig, so the XDG-only key
	// resolves there — again matching git.
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "user.xonly"], &env)).trim(),
		"fromxdg"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "user.xonly"], &env)).trim(),
		"fromxdg"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&home).ok();
}

#[test]
fn unscoped_read_outside_repo_uses_ambient() {
	if !git_supports_sha256() {
		return;
	}
	// A plain directory that is not inside any repository.
	let nonrepo = unique_tmp("gta-config-norepo");
	let n = nonrepo.to_str().unwrap();
	let gdir = unique_tmp("gta-norepo-globalcfg");
	let global = gdir.join("gitconfig");
	std::fs::write(&global, "[user]\n\tname = Ambient\n").unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	// An unscoped read outside any repository still resolves from the ambient stack, as git does.
	assert_eq!(
		ok_stdout(gta_env(n, &["config", "user.name"], &env)).trim(),
		"Ambient"
	);
	// A write, however, has no local file to land in, so it fails (git: "not in a git directory").
	assert!(
		!gta_env(n, &["config", "user.name", "X"], &env)
			.status
			.success()
	);

	std::fs::remove_dir_all(&nonrepo).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn env_config_overrides_merged_read_only() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-envconfig");
	let w = work.to_str().unwrap();
	gta(w, &["config", "user.name", "LocalName"], b"");
	let gdir = unique_tmp("gta-envconfig-global");
	let global = gdir.join("gitconfig");
	std::fs::write(&global, "[user]\n\tname = GlobalName\n").unwrap();
	// git's `-c` propagation form: GIT_CONFIG_COUNT + GIT_CONFIG_KEY_n / GIT_CONFIG_VALUE_n.
	let env = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
		("GIT_CONFIG_COUNT", "1"),
		("GIT_CONFIG_KEY_0", "user.name"),
		("GIT_CONFIG_VALUE_0", "EnvName"),
	];

	// The env entry sits atop the stack for the merged read — above even the local file...
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "user.name"], &env)).trim(),
		"EnvName"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "user.name"], &env)).trim(),
		"EnvName"
	);
	// ...but an explicitly scoped lookup ignores it, reading only that scope's file. git agrees.
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--global", "user.name"], &env)).trim(),
		"GlobalName"
	);
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--local", "user.name"], &env)).trim(),
		"LocalName"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "--global", "user.name"], &env)).trim(),
		"GlobalName"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "--local", "user.name"], &env)).trim(),
		"LocalName"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn malformed_repo_aborts_unscoped_read() {
	if !git_supports_sha256() {
		return;
	}
	// A `.git` file with no `gitdir:` pointer — a corrupted linked-worktree stub. Discovery must
	// abort rather than treat the directory as repo-less and silently read ambient config.
	let bad = unique_tmp("gta-config-badrepo");
	std::fs::write(bad.join(".git"), "garbage, no gitdir pointer\n").unwrap();
	let b = bad.to_str().unwrap();
	let gdir = unique_tmp("gta-badrepo-global");
	let global = gdir.join("gitconfig");
	std::fs::write(&global, "[user]\n\tname = Ambient\n").unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	let out = gta_env(b, &["config", "user.name"], &env);
	assert!(
		!out.status.success(),
		"gta must abort on a malformed .git, not read ambient; stdout={}",
		String::from_utf8_lossy(&out.stdout)
	);
	assert!(
		!git_env(b, &["config", "user.name"], &env).status.success(),
		"git aborts too (oracle)"
	);

	std::fs::remove_dir_all(&bad).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn empty_global_override_reads_no_global() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-emptyglobal");
	let w = work.to_str().unwrap();
	let home = unique_tmp("gta-emptyglobal-home");
	std::fs::write(home.join(".gitconfig"), "[user]\n\tname = RealHome\n").unwrap();
	// An empty GIT_CONFIG_GLOBAL is an explicit "no global config": the real ~/.gitconfig under HOME
	// must NOT be consulted (the value is unset), exactly as git treats it.
	let env = [
		("HOME", home.to_str().unwrap()),
		("GIT_CONFIG_GLOBAL", ""),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	assert!(
		!gta_env(w, &["config", "user.name"], &env).status.success(),
		"empty GIT_CONFIG_GLOBAL must not fall back to ~/.gitconfig"
	);
	assert!(
		!git_env(w, &["config", "user.name"], &env).status.success(),
		"git agrees (oracle)"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&home).ok();
}

#[test]
fn config_env_boolean_and_count_grammar_match_git() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-envgrammar");
	let w = work.to_str().unwrap();
	gta(w, &["config", "user.name", "Loc"], b"");

	// GIT_CONFIG_NOSYSTEM follows git's boolean grammar: integer truthiness (2 / -1 true, 0 false) is
	// accepted, a non-boolean is rejected. Compared purely against git so the outcome is robust to
	// whatever /etc/gitconfig the host has.
	for value in ["2", "-1", "0", "maybe"] {
		let env = [("GIT_CONFIG_NOSYSTEM", value)];
		assert_eq!(
			gta_env(w, &["config", "user.name"], &env).status.success(),
			git_env(w, &["config", "user.name"], &env).status.success(),
			"NOSYSTEM={value}: gta must match git",
		);
	}

	// GIT_CONFIG_COUNT: only the exact empty string is "unset"; a whitespace-padded value is a bogus
	// count and aborts.
	for value in ["", "   ", " 1 ", "1"] {
		let env = [
			("GIT_CONFIG_NOSYSTEM", "1"),
			("GIT_CONFIG_COUNT", value),
			("GIT_CONFIG_KEY_0", "user.extra"),
			("GIT_CONFIG_VALUE_0", "y"),
		];
		assert_eq!(
			gta_env(w, &["config", "user.name"], &env).status.success(),
			git_env(w, &["config", "user.name"], &env).status.success(),
			"COUNT='{value}': gta must match git",
		);
	}

	// GIT_CONFIG_KEY grammar: a malformed propagated `-c` key aborts before the command runs, exactly
	// as git validates it (section alnum/`-`; name letter-then-alnum/`-`; subsection freeform).
	for key in [
		"user.name",
		"user.na_me",
		"a.1",
		"a-b.name",
		"a_b.name",
		"user.sub.name",
		".name",
	] {
		let env = [
			("GIT_CONFIG_NOSYSTEM", "1"),
			("GIT_CONFIG_COUNT", "1"),
			("GIT_CONFIG_KEY_0", key),
			("GIT_CONFIG_VALUE_0", "y"),
		];
		assert_eq!(
			gta_env(w, &["config", "user.name"], &env).status.success(),
			git_env(w, &["config", "user.name"], &env).status.success(),
			"GIT_CONFIG_KEY_0='{key}': gta must match git",
		);
	}

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn bad_config_env_aborts_write_without_mutating() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-badenvwrite");
	let w = work.to_str().unwrap();
	// git parses the config environment at startup and aborts *before* writing when it is malformed;
	// so must gta — the local file must be left untouched.
	let env = [("GIT_CONFIG_NOSYSTEM", "maybe")];
	assert!(
		!gta_env(w, &["config", "user.name", "ShouldNotStick"], &env)
			.status
			.success()
	);
	assert!(
		!git_env(w, &["config", "user.name", "ShouldNotStick"], &env)
			.status
			.success()
	);
	// The write never happened.
	assert!(
		!gta_env(w, &["config", "--local", "user.name"], &[])
			.status
			.success(),
		"a rejected write must not mutate the config"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn empty_config_count_is_unset() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-emptycount");
	let w = work.to_str().unwrap();
	// An empty GIT_CONFIG_COUNT is treated as no command-line config, not a bogus count — a plain
	// operation still succeeds, as git does.
	let env = [("GIT_CONFIG_COUNT", ""), ("GIT_CONFIG_NOSYSTEM", "1")];
	ok_stdout(gta_env(w, &["config", "user.name", "Fine"], &env));
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--local", "user.name"], &env)).trim(),
		"Fine"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn bad_nosystem_value_is_an_error() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-badnosystem");
	let w = work.to_str().unwrap();
	// A non-boolean GIT_CONFIG_NOSYSTEM aborts rather than silently reading system config, as git does.
	let env = [("GIT_CONFIG_NOSYSTEM", "maybe")];
	assert!(
		!gta_env(w, &["config", "user.name"], &env).status.success(),
		"a bad GIT_CONFIG_NOSYSTEM must abort"
	);
	assert!(
		!git_env(w, &["config", "user.name"], &env).status.success(),
		"git aborts too (oracle)"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn relative_override_resolves_against_dash_c_dir() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-relc");
	let w = work.to_str().unwrap();
	// The config file lives inside the repo; the override names it *relatively*. git resolves a
	// relative GIT_CONFIG_GLOBAL against the `-C` directory (here the repo), not the process cwd
	// (the test runner's dir, where `relglobal` does not exist) — and so must gta.
	std::fs::write(work.join("relglobal"), "[user]\n\tname = FromRepoRel\n").unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", "relglobal"),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--global", "user.name"], &env)).trim(),
		"FromRepoRel"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "--global", "user.name"], &env)).trim(),
		"FromRepoRel"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn scoped_keyed_read_tolerates_bad_count_unlike_list_or_local() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-scopedcount");
	let w = work.to_str().unwrap();
	let gdir = unique_tmp("gta-scopedcount-cfg");
	let global = gdir.join("gitconfig");
	std::fs::write(&global, "[user]\n\tname = G\n").unwrap();
	// A bogus command-line config env (GIT_CONFIG_COUNT).
	let bad = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
		("GIT_CONFIG_COUNT", "bogus"),
	];

	// A directly-scoped global *keyed* read answers from the file, ignoring the bogus command-line
	// config — git parses it lazily and never reaches it for this path.
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "--global", "user.name"], &bad)).trim(),
		"G"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "--global", "user.name"], &bad)).trim(),
		"G"
	);
	// But --list and --get-all of that same scope, and any --local read, do validate it — all fail.
	for args in [
		&["config", "--global", "--list"][..],
		&["config", "--global", "--get-all", "user.name"][..],
		&["config", "--local", "user.name"][..],
	] {
		assert!(
			!gta_env(w, args, &bad).status.success(),
			"gta {args:?} must validate the count"
		);
		assert!(
			!git_env(w, args, &bad).status.success(),
			"git {args:?} validates it (oracle)"
		);
	}

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn scoped_read_of_missing_file_errors_but_merged_skips() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-missingscope");
	let w = work.to_str().unwrap();
	let gdir = unique_tmp("gta-missingscope-cfg");
	let missing = gdir.join("nope"); // a file that does not exist
	let env = [
		("GIT_CONFIG_GLOBAL", missing.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	// An explicitly-scoped read of a missing file is fatal, as git treats it.
	assert!(
		!gta_env(w, &["config", "--global", "--list"], &env)
			.status
			.success()
	);
	assert!(
		!git_env(w, &["config", "--global", "--list"], &env)
			.status
			.success()
	);

	// ...but the same missing global is simply skipped in a merged read — a local value still
	// resolves, rather than the whole command aborting.
	gta(w, &["config", "user.name", "Loc"], b"");
	assert_eq!(
		ok_stdout(gta_env(w, &["config", "user.name"], &env)).trim(),
		"Loc"
	);
	assert_eq!(
		ok_stdout(git_env(w, &["config", "user.name"], &env)).trim(),
		"Loc"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn global_scope_requires_home() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-nohome");
	let w = work.to_str().unwrap();
	let xdg = unique_tmp("gta-nohome-xdg");
	std::fs::create_dir_all(xdg.join("git")).unwrap();
	std::fs::write(xdg.join("git/config"), "[user]\n\tname = X\n").unwrap();
	// HOME is cleared by ISOLATION_ENV and not re-supplied here. With no HOME, an explicit `--global`
	// is fatal even though a valid XDG file exists — git refuses with "$HOME not set".
	let env = [
		("XDG_CONFIG_HOME", xdg.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	assert!(
		!gta_env(w, &["config", "--global", "user.name"], &env)
			.status
			.success()
	);
	assert!(
		!git_env(w, &["config", "--global", "user.name"], &env)
			.status
			.success()
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&xdg).ok();
}

#[test]
fn scoped_write_fails_like_git_for_unlockable_paths() {
	if !git_supports_sha256() {
		return;
	}
	let work = init("gta-config-writelock");
	let w = work.to_str().unwrap();

	// A write to /dev/null cannot acquire the `.lock`, so it fails rather than silently discarding —
	// as git does (git config writes go through a lock file).
	let devnull = [
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	assert!(
		!gta_env(w, &["config", "--global", "user.name", "X"], &devnull)
			.status
			.success()
	);
	assert!(
		!git_env(w, &["config", "--global", "user.name", "X"], &devnull)
			.status
			.success()
	);

	// A write below a missing directory also fails, and does not create that directory — matching git,
	// which does not `mkdir -p` for config writes.
	let gdir = unique_tmp("gta-writelock-cfg");
	let missing = gdir.join("nodir").join("cfg");
	let env = [
		("GIT_CONFIG_GLOBAL", missing.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	assert!(
		!gta_env(w, &["config", "--global", "user.name", "X"], &env)
			.status
			.success()
	);
	assert!(
		!gdir.join("nodir").exists(),
		"the missing parent directory must not be created"
	);
	assert!(
		!git_env(w, &["config", "--global", "user.name", "X"], &env)
			.status
			.success()
	);

	// A write whose target is a directory fails the final rename, and must not leave a stale lock.
	let adir = gdir.join("adir");
	std::fs::create_dir(&adir).unwrap();
	let direnv = [
		("GIT_CONFIG_GLOBAL", adir.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	assert!(
		!gta_env(w, &["config", "--global", "user.name", "X"], &direnv)
			.status
			.success()
	);
	assert!(
		!gdir.join("adir.lock").exists(),
		"a failed write must not leave a stale .lock"
	);

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[cfg(unix)]
#[test]
fn scoped_write_preserves_symlink_and_mode() {
	if !git_supports_sha256() {
		return;
	}
	use std::os::unix::fs::PermissionsExt;
	let work = init("gta-config-symlinkmode");
	let w = work.to_str().unwrap();
	let gdir = unique_tmp("gta-symlinkmode-cfg");

	// A symlinked config file: the write must update the real target and leave the link in place, as
	// git does — clobbering it would break a dotfile-managed `~/.gitconfig`.
	let real = gdir.join("real");
	std::fs::write(&real, "[user]\n\tname = Old\n").unwrap();
	let link = gdir.join("link");
	std::os::unix::fs::symlink(&real, &link).unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", link.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	ok_stdout(gta_env(
		w,
		&["config", "--global", "user.name", "New"],
		&env,
	));
	assert!(
		std::fs::symlink_metadata(&link)
			.unwrap()
			.file_type()
			.is_symlink(),
		"the symlink must be preserved"
	);
	assert!(
		std::fs::read_to_string(&real).unwrap().contains("New"),
		"the real target must be updated through the link"
	);

	// A *broken* symlink (target not yet created — a dotfile-managed `~/.gitconfig` before first use)
	// resolves to its target: the write creates it and leaves the link, rather than clobbering the link.
	let broken_target = gdir.join("broken-target");
	let broken_link = gdir.join("broken-link");
	std::os::unix::fs::symlink(&broken_target, &broken_link).unwrap();
	let benv = [
		("GIT_CONFIG_GLOBAL", broken_link.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	ok_stdout(gta_env(
		w,
		&["config", "--global", "user.name", "Fresh"],
		&benv,
	));
	assert!(
		std::fs::symlink_metadata(&broken_link)
			.unwrap()
			.file_type()
			.is_symlink(),
		"the broken symlink must be preserved"
	);
	assert!(
		std::fs::read_to_string(&broken_target)
			.unwrap()
			.contains("Fresh"),
		"the symlink's target must be created and written"
	);

	// A restrictive file mode (a private `0600` config) must survive the write, not be reset to umask.
	let private = gdir.join("private");
	std::fs::write(&private, "[user]\n\tname = Old\n").unwrap();
	std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
	let penv = [
		("GIT_CONFIG_GLOBAL", private.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];
	ok_stdout(gta_env(
		w,
		&["config", "--global", "user.name", "New"],
		&penv,
	));
	let mode = std::fs::metadata(&private).unwrap().permissions().mode() & 0o777;
	assert_eq!(mode, 0o600, "the file mode must be preserved");

	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&gdir).ok();
}

#[test]
fn config_rejects_missing_dash_c_dir() {
	if !git_supports_sha256() {
		return;
	}
	let gdir = unique_tmp("gta-badc-cfg");
	let global = gdir.join("gitconfig");
	std::fs::write(&global, "[user]\n\tname = G\n").unwrap();
	let missing = gdir.join("does-not-exist");
	let m = missing.to_str().unwrap();
	let env = [
		("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
		("GIT_CONFIG_NOSYSTEM", "1"),
	];

	// A missing `-C` directory aborts every config operation — even `--global`, which needs no
	// repository — because git applies `-C` before reading any config. gta must not "succeed" from
	// ambient config in that case.
	for args in [
		&["config", "user.name"][..],
		&["config", "--global", "user.name"][..],
	] {
		assert!(
			!gta_env(m, args, &env).status.success(),
			"gta {args:?} must reject a missing -C directory"
		);
		assert!(
			!git_env(m, args, &env).status.success(),
			"git {args:?} rejects it too (oracle)"
		);
	}

	std::fs::remove_dir_all(&gdir).ok();
}

/// A global `core.logallrefupdates` governs the engine's reflog writes, not just a local setting.
/// The discriminator: a *non-bare* repo defaults to reflogs **enabled**, so a global `false` can only
/// suppress `.git/logs/HEAD` if the reflog policy reads git's merged config. Without global-awareness
/// gitana would see no local setting, fall back to the non-bare default, and wrongly write the reflog.
/// Cross-checked against stock git, which reads the same merged key.
#[test]
fn global_logallrefupdates_governs_core_reflog_writes() {
	let home = unique_tmp("gta-global-reflog-home");
	let global = home.join(".gitconfig");
	std::fs::write(
		&global,
		"[user]\n\tname = A U Thor\n\temail = a@example.com\n[core]\n\tlogallrefupdates = false\n",
	)
	.unwrap();
	let env = &[("GIT_CONFIG_GLOBAL", global.to_str().unwrap())];

	// gta and stock git, given the same global config, must reach the same reflog decision.
	for tool in ["gta", "git"] {
		let work = unique_tmp(&format!("gta-global-reflog-{tool}"));
		let w = work.to_str().unwrap();
		std::fs::write(work.join("a.txt"), b"hello\n").unwrap();
		let run = |args: &[&str]| {
			let out = if tool == "gta" {
				gta_env(w, args, env)
			} else {
				git_env(w, args, env)
			};
			assert!(
				out.status.success(),
				"{tool} {args:?} failed: {}",
				String::from_utf8_lossy(&out.stderr)
			);
		};
		run(&["init"]);
		run(&["add", "a.txt"]);
		run(&["commit", "-m", "c"]);
		assert!(
			!work.join(".git/logs/HEAD").exists(),
			"{tool}: a global core.logallrefupdates=false must suppress the HEAD reflog"
		);
		std::fs::remove_dir_all(&work).ok();
	}
	std::fs::remove_dir_all(&home).ok();
}

/// Regression guard for the raw-local boundary: repository *format* is repo-local identity, read raw
/// from `.git/config` (never the merged stack), so a global `extensions.objectformat = sha256` must
/// not make a sha1 repo behave as sha256. The effective-config plumbing must never reach the
/// format-detection path.
#[test]
fn global_objectformat_does_not_change_repo_format() {
	let home = unique_tmp("gta-global-objfmt-home");
	let global = home.join(".gitconfig");
	std::fs::write(&global, "[extensions]\n\tobjectformat = sha256\n").unwrap();
	let env = &[("GIT_CONFIG_GLOBAL", global.to_str().unwrap())];

	let work = unique_tmp("gta-global-objfmt");
	let w = work.to_str().unwrap();
	assert!(
		gta_env(w, &["init", "--object-format=sha1"], env)
			.status
			.success(),
		"init --object-format=sha1 should succeed"
	);
	std::fs::write(work.join("a.txt"), b"hi\n").unwrap();
	// hash-object must produce a **sha1** (40-hex) id: the global sha256 override must not leak into
	// the repo's format. A 64-hex id would mean the format read consulted the merged config.
	let oid = ok_stdout(gta_env(w, &["hash-object", "a.txt"], env));
	assert_eq!(
		oid.trim().len(),
		40,
		"expected a sha1 (40-hex) object id, got {oid:?} — global objectformat leaked into repo format"
	);
	std::fs::remove_dir_all(&work).ok();
	std::fs::remove_dir_all(&home).ok();
}

/// The environment an isolated config test clears before applying its own, so a resolved identity or
/// value comes only from the config files under test — never from the runner's identity env or its
/// real `~/.gitconfig` / system config. Tests re-supply exactly what they want (`HOME`,
/// `GIT_CONFIG_GLOBAL`, …).
const ISOLATION_ENV: [&str; 11] = [
	"GIT_AUTHOR_NAME",
	"GIT_AUTHOR_EMAIL",
	"GIT_COMMITTER_NAME",
	"GIT_COMMITTER_EMAIL",
	"GIT_COMMITTER_DATE",
	"GIT_CONFIG_GLOBAL",
	"GIT_CONFIG_SYSTEM",
	"GIT_CONFIG_NOSYSTEM",
	"GIT_CONFIG_COUNT",
	"XDG_CONFIG_HOME",
	// Cleared so a test's global discovery is driven only by what it re-supplies (HOME or
	// GIT_CONFIG_GLOBAL), never the runner's real home.
	"HOME",
];

/// Run `gta` with the config/identity environment cleared and `envs` applied, so config files (not
/// env overrides or the runner's own config) drive identity and config discovery.
fn gta_env(dir: &str, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
	let mut cmd = assert_cmd::Command::cargo_bin("gta").unwrap();
	cmd.args(["-C", dir]).args(args);
	for var in ISOLATION_ENV {
		cmd.env_remove(var);
	}
	for (key, value) in envs {
		cmd.env(key, value);
	}
	cmd.output().expect("run gta")
}

/// The stock-git oracle counterpart to [`gta_env`].
fn git_env(dir: &str, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
	let mut cmd = Command::new("git");
	cmd.args(["-C", dir]).args(args);
	for var in ISOLATION_ENV {
		cmd.env_remove(var);
	}
	for (key, value) in envs {
		cmd.env(key, value);
	}
	cmd.output().expect("run git")
}

/// Assert a command succeeded and return its stdout as a string.
fn ok_stdout(out: std::process::Output) -> String {
	assert!(
		out.status.success(),
		"command failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("stdout utf8")
}

fn gta(dir: &str, args: &[&str], stdin: &[u8]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.write_stdin(stdin.to_vec())
		.output()
		.expect("run gta");
	assert!(
		out.status.success(),
		"gta {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("gta stdout utf8")
}

fn gta_fail(dir: &str, args: &[&str]) -> String {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir])
		.args(args)
		.output()
		.expect("run gta");
	assert!(!out.status.success(), "gta {args:?} unexpectedly succeeded");
	String::from_utf8(out.stderr).expect("gta stderr utf8")
}

fn git(dir: &str, args: &[&str]) -> String {
	let mut full = vec!["-C", dir];
	full.extend_from_slice(args);
	let out = Command::new("git").args(&full).output().expect("run git");
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout utf8")
}

fn unique_tmp(tag: &str) -> PathBuf {
	use std::sync::atomic::{AtomicU64, Ordering};
	static SEQ: AtomicU64 = AtomicU64::new(0);
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let dir = std::env::temp_dir().join(format!("gta-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe-config");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
