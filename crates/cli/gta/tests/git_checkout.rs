//! `gta checkout` end-to-end: branch switching still works, and path restore (from the
//! index or a tree-ish) restores files without moving `HEAD`, cross-checked against real git.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn checkout_restores_paths_without_moving_head() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-checkout-restore");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"A1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");
	let c1 = git(w, &["rev-parse", "HEAD"]).trim().to_owned();

	std::fs::write(work.join("a.txt"), b"A2\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "two");

	// `checkout -- <path>` restores the working tree from the index, discarding edits.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	gta(w, &["checkout", "--", "a.txt"], b"");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A2\n");

	// `checkout <tree-ish> -- <path>` restores both the working tree and the index.
	gta(w, &["checkout", &c1, "--", "a.txt"], b"");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"A1\n");
	assert_eq!(
		git(w, &["diff", "--cached", "--name-only"]).trim(),
		"a.txt",
		"the tree content is staged"
	);

	// HEAD never moved during path restore.
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn checkout_without_paths_still_switches_branches() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-checkout-switch");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"1\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");

	gta(w, &["branch", "feature"], b"");
	gta(w, &["checkout", "feature"], b"");
	assert_eq!(
		git(w, &["symbolic-ref", "HEAD"]).trim(),
		"refs/heads/feature"
	);

	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn checkout_restore_is_relative_to_cwd() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-checkout-subdir");
	let w = work.to_str().unwrap();
	let sub = work.join("sub");
	let s = sub.to_str().unwrap();
	gta(w, &["init"], b"");

	std::fs::write(work.join("a.txt"), b"ROOT\n").unwrap();
	std::fs::create_dir_all(&sub).unwrap();
	std::fs::write(sub.join("a.txt"), b"SUB\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "one");

	// `gta -C sub checkout -- a.txt` restores sub/a.txt, leaving the root file dirty,
	// matching `git -C sub checkout -- a.txt`.
	std::fs::write(work.join("a.txt"), b"dirty\n").unwrap();
	std::fs::write(sub.join("a.txt"), b"dirty\n").unwrap();
	gta(s, &["checkout", "--", "a.txt"], b"");
	assert_eq!(std::fs::read(sub.join("a.txt")).unwrap(), b"SUB\n");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"dirty\n");

	// `gta -C sub checkout -- .` restores only entries under sub/, like git does.
	std::fs::write(sub.join("a.txt"), b"dirty\n").unwrap();
	gta(s, &["checkout", "--", "."], b"");
	assert_eq!(std::fs::read(sub.join("a.txt")).unwrap(), b"SUB\n");
	assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"dirty\n");

	std::fs::remove_dir_all(&work).ok();
}

#[test]
#[cfg(unix)]
fn checkout_restore_resolves_symlinked_cwd() {
	if !git_supports_sha256() {
		return;
	}
	let work = unique_tmp("gta-checkout-symlink");
	let w = work.to_str().unwrap();
	let sub = work.join("sub");
	gta(w, &["init"], b"");

	std::fs::create_dir_all(&sub).unwrap();
	std::fs::write(sub.join("a.txt"), b"SUB\n").unwrap();
	std::os::unix::fs::symlink("sub", work.join("linksub")).unwrap();
	git(w, &["add", "sub"]);
	commit(w, "one");

	// `-C linksub` (a symlink to `sub`) must resolve to `sub`, so `a.txt` means `sub/a.txt`.
	std::fs::write(sub.join("a.txt"), b"dirty\n").unwrap();
	let link = work.join("linksub");
	gta(link.to_str().unwrap(), &["checkout", "--", "a.txt"], b"");
	assert_eq!(std::fs::read(sub.join("a.txt")).unwrap(), b"SUB\n");
	// The tracked path is `sub/a.txt`, never `linksub/a.txt`.
	assert!(
		git(w, &["ls-files"])
			.lines()
			.all(|l| !l.starts_with("linksub/"))
	);

	std::fs::remove_dir_all(&work).ok();
}

fn commit(dir: &str, msg: &str) {
	git(
		dir,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"-m",
			msg,
		],
	);
}

#[test]
fn switch_c_that_fails_checkout_creates_no_branch() {
	// A `switch -c <name> <start>` whose checkout aborts (an in-the-way untracked file) must not leave
	// the new branch behind — git validates and updates the working tree before publishing the branch
	// (probed vs git 2.55: `switch -c <name> <other>` that fails creates nothing).
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-switch-orphan");
	let w = work.to_str().unwrap();
	gta(w, &["init"], b"");
	std::fs::write(work.join("blocker"), b"A\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "with-blocker");
	let with_blocker = git(w, &["rev-parse", "HEAD"]).trim().to_owned();
	git(w, &["rm", "-q", "blocker"]);
	commit(w, "no-blocker");
	// An untracked `blocker` in the way of the start-point's tracked `blocker`.
	std::fs::write(work.join("blocker"), b"LOCAL\n").unwrap();

	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", w, "switch", "-c", "newbr", &with_blocker])
		.output()
		.expect("run gta");
	assert!(
		!out.status.success(),
		"switch -c over an in-the-way untracked file must fail"
	);
	assert!(
		git(w, &["branch", "--list", "newbr"]).trim().is_empty(),
		"a failed switch -c must not create the branch"
	);
	assert_eq!(
		std::fs::read(work.join("blocker")).unwrap(),
		b"LOCAL\n",
		"the untracked file is untouched"
	);
	// Oracle: git refuses too and creates no branch.
	let gout = Command::new("git")
		.args(["-C", w, "switch", "-c", "gitnewbr", &with_blocker])
		.output()
		.expect("run git");
	assert!(!gout.status.success(), "sanity: git also refuses");
	assert!(
		git(w, &["branch", "--list", "gitnewbr"]).trim().is_empty(),
		"sanity: git creates no branch either"
	);

	std::fs::remove_dir_all(&work).ok();
}

/// A non-force `switch` is git's two-tree merge (`read-tree -m -u`): local staged/unstaged work that does
/// not conflict with the branch change is carried across the switch, and a real conflict is refused —
/// rather than the target silently overwriting or dropping it. Each scenario builds an identical repo for
/// `gta` and for `git` (at `base`, with a scenario-specific `other` branch and staged state), switches to
/// `other`, and asserts byte-for-byte parity of exit status, index, and working tree.
#[test]
fn switch_two_way_merges_staged_changes_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// (name, build the scenario given the work dir — starting from a repo already committed at `base`).
	type Setup = fn(&str);
	let scenarios: &[(&str, Setup)] = &[
		("staged-add: target has no bar", |w| {
			other_same(w);
			std::fs::write(format!("{w}/bar"), b"NEW\n").unwrap();
			git(w, &["add", "bar"]);
		}),
		("staged-add: target adds bar diff", |w| {
			other_add(w);
			std::fs::write(format!("{w}/bar"), b"NEW\n").unwrap();
			git(w, &["add", "bar"]);
		}),
		("staged-mod: target keeps foo", |w| {
			other_same(w);
			std::fs::write(format!("{w}/foo"), b"S\n").unwrap();
			git(w, &["add", "foo"]);
		}),
		("staged-mod: target mods foo diff", |w| {
			other_mod(w);
			std::fs::write(format!("{w}/foo"), b"S\n").unwrap();
			git(w, &["add", "foo"]);
		}),
		("staged-mod: target mods foo same", |w| {
			other_mod(w);
			std::fs::write(format!("{w}/foo"), b"fooT\n").unwrap();
			git(w, &["add", "foo"]);
		}),
		("staged-mod: target deletes foo", |w| {
			other_del(w);
			std::fs::write(format!("{w}/foo"), b"S\n").unwrap();
			git(w, &["add", "foo"]);
		}),
		("staged-del: target keeps foo", |w| {
			other_same(w);
			git(w, &["rm", "-q", "foo"]);
		}),
		("staged-del: target mods foo", |w| {
			other_mod(w);
			git(w, &["rm", "-q", "foo"]);
		}),
		("staged-del: target deletes foo", |w| {
			other_del(w);
			git(w, &["rm", "-q", "foo"]);
		}),
		("unstaged: target mods foo", |w| {
			other_mod(w);
			std::fs::write(format!("{w}/foo"), b"WT\n").unwrap();
		}),
		("unstaged: target keeps foo", |w| {
			other_same(w);
			std::fs::write(format!("{w}/foo"), b"WT\n").unwrap();
		}),
		("staged==target + unstaged delete", |w| {
			other_mod(w);
			std::fs::write(format!("{w}/foo"), b"fooT\n").unwrap();
			git(w, &["add", "foo"]);
			std::fs::remove_file(format!("{w}/foo")).unwrap();
		}),
		("staged==target + unstaged modify", |w| {
			other_mod(w);
			std::fs::write(format!("{w}/foo"), b"fooT\n").unwrap();
			git(w, &["add", "foo"]);
			std::fs::write(format!("{w}/foo"), b"WT\n").unwrap();
		}),
	];

	for (name, setup) in scenarios {
		let build = |tag: &str| -> PathBuf {
			let work = unique_tmp(&format!("gta-switch2way-{tag}"));
			let w = work.to_str().unwrap();
			git(
				w,
				&["init", "-q", "-b", "main", "--object-format=sha256", "."],
			);
			std::fs::write(work.join("foo"), b"base\n").unwrap();
			std::fs::write(work.join("keep"), b"K\n").unwrap();
			git(w, &["add", "."]);
			commit(w, "base");
			setup(w);
			work
		};
		let a = build("gta");
		let b = build("git");
		let ours = switch_try_gta(a.to_str().unwrap());
		let theirs = switch_try_git(b.to_str().unwrap());
		assert_eq!(
			ours.0, theirs.0,
			"{name}: exit-success parity (gta {}, git {})",
			ours.0, theirs.0
		);
		assert_eq!(snapshot(&a), snapshot(&b), "{name}: index+worktree parity");
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

#[test]
fn switch_refuses_while_operation_in_progress_like_git() {
	// A merge left in progress (`MERGE_HEAD` present) but with NO unresolved conflict stages — e.g. after
	// `merge --no-commit` — must still block a switch: git refuses "cannot switch branch while merging", so
	// the operation state cannot be finished on the wrong branch. gta must refuse too.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-switch-op-inprogress");
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(work.join("f"), b"base\n").unwrap();
	git(w, &["add", "f"]);
	commit(w, "base");
	git(w, &["switch", "-q", "-c", "feat"]);
	std::fs::write(work.join("g"), b"feat\n").unwrap();
	git(w, &["add", "g"]);
	commit(w, "feat");
	git(w, &["switch", "-q", "main"]);
	git(w, &["branch", "sibling"]);
	// A conflict-free merge left uncommitted: MERGE_HEAD present, index clean.
	let _ = Command::new("git")
		.args([
			"-C",
			w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"merge",
			"--no-commit",
			"--no-ff",
			"feat",
		])
		.output();
	assert!(
		work.join(".git/MERGE_HEAD").exists(),
		"sanity: a merge is in progress"
	);

	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", w, "switch", "sibling"])
		.output()
		.expect("run gta");
	assert!(
		!out.status.success(),
		"switch while merging must be refused"
	);
	assert!(
		!Command::new("git")
			.args(["-C", w, "switch", "sibling"])
			.output()
			.unwrap()
			.status
			.success(),
		"sanity: git refuses too"
	);
	// `--force` overrides worktree-overwrite protection, NOT the operation-state guard: `git switch -f`
	// also refuses while merging (probed vs git 2.55), so `gta switch --force` must too.
	assert!(
		!assert_cmd::Command::cargo_bin("gta")
			.unwrap()
			.args(["-C", w, "switch", "--force", "sibling"])
			.output()
			.unwrap()
			.status
			.success(),
		"switch --force while merging must be refused too"
	);
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");
	std::fs::remove_dir_all(&work).ok();
}

/// A rebase started by *stock git* keeps its state under `rebase-merge/`, not gitana's flat `REBASE_*`
/// files, and once its conflicts are staged the index has no unmerged entries — so `gta switch` must
/// detect the directory directly and refuse, as git does ("cannot switch branch while rebasing").
#[test]
fn switch_refuses_during_stock_git_rebase_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-switch-stock-rebase");
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(work.join("f"), b"1\n").unwrap();
	git(w, &["add", "f"]);
	commit(w, "c1");
	git(w, &["switch", "-q", "-c", "topic"]);
	std::fs::write(work.join("f"), b"topic\n").unwrap();
	git(w, &["add", "f"]);
	commit(w, "ctopic");
	git(w, &["switch", "-q", "main"]);
	std::fs::write(work.join("f"), b"main\n").unwrap();
	git(w, &["add", "f"]);
	commit(w, "cmain");
	git(w, &["branch", "sibling"]);
	// Start a stock-git rebase that conflicts, then STAGE the resolution: `rebase-merge/` persists but the
	// index has no unmerged stages, so only detecting the directory catches it.
	let _ = Command::new("git")
		.args([
			"-C",
			w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"rebase",
			"topic",
		])
		.output();
	std::fs::write(work.join("f"), b"resolved\n").unwrap();
	git(w, &["add", "f"]);
	assert!(
		work.join(".git/rebase-merge").is_dir(),
		"sanity: a stock-git rebase is in progress"
	);
	assert!(
		git(w, &["ls-files", "-u"]).trim().is_empty(),
		"sanity: no unmerged stages remain"
	);
	assert!(
		!assert_cmd::Command::cargo_bin("gta")
			.unwrap()
			.args(["-C", w, "switch", "sibling"])
			.output()
			.unwrap()
			.status
			.success(),
		"switch during a stock-git rebase must be refused"
	);
	std::fs::remove_dir_all(&work).ok();
}

/// A sparse (out-of-cone) path that HEAD tracks and the target modifies, but whose index entry is
/// staged-DELETED: git refuses the switch ("local changes would be overwritten") rather than reinstate
/// the target blob and silently drop the staged deletion. The sparse-addition exemption must not
/// misclassify a staged deletion (present in HEAD) as a brand-new addition.
#[test]
fn switch_refuses_sparse_staged_deletion_overwrite_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-sparse-del-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::create_dir(work.join("in")).unwrap();
		std::fs::create_dir(work.join("out")).unwrap();
		std::fs::write(work.join("in/a"), b"a").unwrap();
		std::fs::write(work.join("out/p"), b"p").unwrap();
		git(w, &["add", "-A"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		std::fs::write(work.join("out/p"), b"pMOD").unwrap();
		git(w, &["add", "out/p"]);
		commit(w, "mod"); // target modifies out/p
		git(w, &["switch", "-q", "main"]);
		git(w, &["sparse-checkout", "set", "--no-cone", "in"]);
		git(w, &["update-index", "--force-remove", "out/p"]); // stage a DELETION of out/p
		work
	};
	let a = build("gta");
	let b = build("git");
	assert_eq!(
		switch_try_gta_to(a.to_str().unwrap(), "other"),
		switch_try_git_to(b.to_str().unwrap(), "other"),
		"exit parity"
	);
	assert!(
		!switch_try_git_to(b.to_str().unwrap(), "other"),
		"sanity: git refuses the overwrite"
	);
	assert_eq!(
		git(a.to_str().unwrap(), &["symbolic-ref", "HEAD"]).trim(),
		"refs/heads/main",
		"gta must not move HEAD"
	);
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

#[test]
fn switch_refuses_recreated_nonempty_dir_at_deletion_like_git() {
	// A satisfied deletion (HEAD file `p`, both index and target delete it) whose working path is recreated
	// as a directory holding non-ignored untracked files: git aborts ("would lose untracked files"). An empty
	// or wholly-ignored directory would be fine, but this one is not — gta must refuse.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-switch-recreated-dir");
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(work.join("p"), b"X").unwrap();
	git(w, &["add", "p"]);
	commit(w, "base");
	git(w, &["switch", "-q", "-c", "other"]);
	git(w, &["rm", "-q", "p"]);
	commit(w, "t");
	git(w, &["switch", "-q", "main"]);
	git(w, &["rm", "-q", "p"]);
	std::fs::create_dir(work.join("p")).unwrap();
	std::fs::write(work.join("p/untracked"), b"u").unwrap();

	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", w, "switch", "other"])
		.output()
		.expect("run gta");
	assert!(
		!out.status.success(),
		"a recreated dir with untracked files must block the switch"
	);
	assert!(
		!Command::new("git")
			.args(["-C", w, "switch", "other"])
			.output()
			.unwrap()
			.status
			.success(),
		"sanity: git refuses too"
	);
	assert_eq!(
		std::fs::read(work.join("p/untracked")).unwrap(),
		b"u",
		"the untracked file survives"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn switch_refuses_unmerged_index_like_git() {
	// A two-tree-merge `switch` must not move `HEAD` while the index has unresolved conflict stages — even
	// to a branch at the same commit — or the unmerged state would attach to the wrong branch. git refuses
	// ("cannot switch branch while merging" / "resolve your current index first"); gta must too.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-switch-unmerged");
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(work.join("f"), b"base\n").unwrap();
	git(w, &["add", "."]);
	commit(w, "base");
	git(w, &["switch", "-q", "-c", "ours"]);
	std::fs::write(work.join("f"), b"OURS\n").unwrap();
	git(w, &["add", "f"]);
	commit(w, "ours");
	git(w, &["switch", "-q", "main"]);
	git(w, &["switch", "-q", "-c", "theirs"]);
	std::fs::write(work.join("f"), b"THEIRS\n").unwrap();
	git(w, &["add", "f"]);
	commit(w, "theirs");
	git(w, &["switch", "-q", "ours"]);
	let _ = Command::new("git")
		.args([
			"-C",
			w,
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"merge",
			"theirs",
		])
		.output();
	git(w, &["branch", "sibling", "ours"]);
	assert!(
		git(w, &["status", "--porcelain"]).contains("UU f"),
		"sanity: the index is unmerged"
	);

	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", w, "switch", "sibling"])
		.output()
		.expect("run gta");
	assert!(
		!out.status.success(),
		"switch with an unmerged index must be refused"
	);
	// git refuses too, and HEAD stays on `ours`.
	let gout = Command::new("git")
		.args(["-C", w, "switch", "sibling"])
		.output()
		.expect("run git");
	assert!(!gout.status.success(), "sanity: git refuses too");
	assert_eq!(git(w, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/ours");
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn switch_replaces_clean_directory_with_file_like_git() {
	// A branch switch whose target replaces a tracked subtree (`thing/child`) with a file at `thing` must
	// succeed when the directory is clean — git replaces it; the two-tree merge must not reject the clean
	// directory as a same-slot local change.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-dir2file-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::create_dir(work.join("thing")).unwrap();
		std::fs::write(work.join("thing/child"), b"c\n").unwrap();
		git(w, &["add", "."]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		git(w, &["rm", "-q", "-r", "thing"]);
		std::fs::write(work.join("thing"), b"F\n").unwrap();
		git(w, &["add", "thing"]);
		commit(w, "t");
		git(w, &["switch", "-q", "main"]);
		work
	};
	let a = build("gta");
	let b = build("git");
	let ours = switch_try_gta(a.to_str().unwrap());
	let theirs = switch_try_git(b.to_str().unwrap());
	assert_eq!(ours.0, theirs.0, "dir->file switch exit parity");
	assert!(
		ours.0,
		"the clean directory->file switch must succeed like git"
	);
	assert_eq!(
		snapshot(&a),
		snapshot(&b),
		"dir->file index+worktree parity"
	);
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

#[test]
fn switch_resolves_staged_directory_file_collisions_like_git() {
	// A two-tree merge lets the incoming target win a directory/file conflict: switching to a branch that
	// adds a file `thing` while the index stages `thing/child` (and the inverse) must drop the colliding
	// staged entry so the index stays valid — git does this and `write-tree` succeeds; leaving both would be
	// an invalid file/directory index.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// (name, target-adds, staged-local) — the target commits `add`, then the working repo stages `stage`.
	type DfCase = (&'static str, fn(&str), fn(&str));
	let cases: &[DfCase] = &[
		(
			"target-file-vs-staged-subtree",
			|w| {
				std::fs::write(format!("{w}/thing"), b"T\n").unwrap();
				git(w, &["add", "thing"]);
			},
			|w| {
				std::fs::create_dir(format!("{w}/thing")).unwrap();
				std::fs::write(format!("{w}/thing/child"), b"c\n").unwrap();
				git(w, &["add", "thing/child"]);
			},
		),
		(
			"target-subtree-vs-staged-file",
			|w| {
				std::fs::create_dir(format!("{w}/thing")).unwrap();
				std::fs::write(format!("{w}/thing/child"), b"c\n").unwrap();
				git(w, &["add", "thing/child"]);
			},
			|w| {
				std::fs::write(format!("{w}/thing"), b"T\n").unwrap();
				git(w, &["add", "thing"]);
			},
		),
	];
	for (name, add, stage) in cases {
		let build = |tag: &str| -> PathBuf {
			let work = unique_tmp(&format!("gta-switch-df-{name}-{tag}"));
			let w = work.to_str().unwrap();
			git(
				w,
				&["init", "-q", "-b", "main", "--object-format=sha256", "."],
			);
			std::fs::write(work.join("a"), b"a\n").unwrap();
			git(w, &["add", "a"]);
			commit(w, "base");
			git(w, &["switch", "-q", "-c", "other"]);
			add(w);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			stage(w);
			work
		};
		let a = build("gta");
		let b = build("git");
		let ours = switch_try_gta(a.to_str().unwrap());
		let theirs = switch_try_git(b.to_str().unwrap());
		assert_eq!(ours.0, theirs.0, "{name}: exit parity");
		assert_eq!(snapshot(&a), snapshot(&b), "{name}: index+worktree parity");
		assert!(
			Command::new("git")
				.args(["-C", a.to_str().unwrap(), "write-tree"])
				.output()
				.unwrap()
				.status
				.success(),
			"{name}: the resulting index must be a valid tree"
		);
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

#[test]
fn switch_refuses_dirty_directory_file_collision_like_git() {
	// A directory/file collision the target would resolve is discarded only when the colliding staged path
	// is clean; if it carries an unstaged edit, git aborts rather than lose that edit, and gta must too.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let work = unique_tmp("gta-switch-df-dirty");
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(work.join("a"), b"a\n").unwrap();
	git(w, &["add", "a"]);
	commit(w, "base");
	git(w, &["switch", "-q", "-c", "other"]);
	std::fs::write(work.join("thing"), b"T\n").unwrap();
	git(w, &["add", "thing"]);
	commit(w, "t");
	git(w, &["switch", "-q", "main"]);
	std::fs::create_dir(work.join("thing")).unwrap();
	std::fs::write(work.join("thing/child"), b"c\n").unwrap();
	git(w, &["add", "thing/child"]);
	std::fs::write(work.join("thing/child"), b"DIRTY\n").unwrap(); // unstaged edit

	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", w, "switch", "other"])
		.output()
		.expect("run gta");
	assert!(
		!out.status.success(),
		"a dirty D/F collision must be refused"
	);
	assert_eq!(
		std::fs::read(work.join("thing/child")).unwrap(),
		b"DIRTY\n",
		"the unstaged edit must survive"
	);
	// git refuses too.
	assert!(
		!Command::new("git")
			.args(["-C", w, "switch", "other"])
			.output()
			.unwrap()
			.status
			.success(),
		"sanity: git refuses too"
	);
	std::fs::remove_dir_all(&work).ok();
}

#[test]
fn switch_carries_staged_work_from_unborn_head_like_git() {
	// Staged work created on an unborn (orphan) HEAD must be carried across a switch to an existing branch,
	// exactly as git's two-tree merge from the empty tree does — not dropped by an authoritative checkout.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-unborn-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::write(work.join("m"), b"m\n").unwrap();
		git(w, &["add", "m"]);
		commit(w, "base");
		git(w, &["switch", "-q", "--orphan", "fresh"]);
		std::fs::write(work.join("bar"), b"B\n").unwrap();
		git(w, &["add", "bar"]);
		work
	};
	let a = build("gta");
	let b = build("git");
	let ours = switch_try_gta_to(a.to_str().unwrap(), "main");
	let theirs = switch_try_git_to(b.to_str().unwrap(), "main");
	assert_eq!(ours, theirs, "unborn-HEAD switch exit parity");
	assert!(ours, "the switch must succeed");
	assert_eq!(
		snapshot(&a),
		snapshot(&b),
		"unborn-HEAD staged work must be carried"
	);
	assert!(a.join("bar").exists(), "staged bar must survive the switch");
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

#[test]
fn switch_rebuilds_missing_index_like_git() {
	// A *missing* `.git/index` (not merely empty) has no staged state: a switch must rebuild from the target
	// like a full checkout — repopulating an absent file, but refusing an in-the-way untracked one — matching
	// git, not silently succeeding on the empty tree diff.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	for (tag, remove_file) in [("present", false), ("absent", true)] {
		let build = |tool: &str| -> PathBuf {
			let work = unique_tmp(&format!("gta-switch-noindex-{tag}-{tool}"));
			let w = work.to_str().unwrap();
			git(
				w,
				&["init", "-q", "-b", "main", "--object-format=sha256", "."],
			);
			std::fs::write(work.join("foo"), b"foo\n").unwrap();
			git(w, &["add", "foo"]);
			commit(w, "base");
			git(w, &["branch", "other"]);
			std::fs::remove_file(work.join(".git/index")).unwrap();
			if remove_file {
				std::fs::remove_file(work.join("foo")).unwrap();
			}
			work
		};
		let a = build("gta");
		let b = build("git");
		let ours = switch_try_gta(a.to_str().unwrap());
		let theirs = switch_try_git(b.to_str().unwrap());
		assert_eq!(ours.0, theirs.0, "missing-index ({tag}) exit parity");
		assert_eq!(snapshot(&a), snapshot(&b), "missing-index ({tag}) parity");
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

#[test]
fn switch_refuses_recreated_file_over_satisfied_deletion_like_git() {
	// A path staged for deletion, recreated as a non-ignored untracked file, whose target branch also deletes
	// it: the index and target agree (a "satisfied" deletion), but the recreated file is in the way — git
	// refuses rather than move HEAD and leave it, and gta must too.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-satdel-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::write(work.join("foo"), b"f\n").unwrap();
		git(w, &["add", "foo"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		git(w, &["rm", "-q", "foo"]);
		commit(w, "t");
		git(w, &["switch", "-q", "main"]);
		git(w, &["rm", "-q", "foo"]);
		std::fs::write(work.join("foo"), b"RECREATED\n").unwrap();
		work
	};
	let a = build("gta");
	let b = build("git");
	assert_eq!(
		switch_try_gta(a.to_str().unwrap()).0,
		switch_try_git(b.to_str().unwrap()).0,
		"exit parity"
	);
	assert!(
		!switch_try_gta(a.to_str().unwrap()).0,
		"the recreated file must block the switch"
	);
	assert_eq!(snapshot(&a), snapshot(&b), "index+worktree parity");
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

#[test]
#[cfg(unix)]
fn switch_replaces_staged_symlink_directory_collision_like_git() {
	// A staged symlink `thing` colliding with a target `thing/child`: git replaces the (clean) symlink and
	// switches; the two-tree merge must clear the symlink from the working tree before writing descendants,
	// not abort on it as an unsafe ancestor.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-symdf-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::write(work.join("keep"), b"k\n").unwrap();
		git(w, &["add", "keep"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		std::fs::create_dir(work.join("thing")).unwrap();
		std::fs::write(work.join("thing/child"), b"c\n").unwrap();
		git(w, &["add", "thing/child"]);
		commit(w, "t");
		git(w, &["switch", "-q", "main"]);
		std::os::unix::fs::symlink("keep", work.join("thing")).unwrap();
		git(w, &["add", "thing"]);
		work
	};
	let a = build("gta");
	let b = build("git");
	// Whether the staged-symlink/target-subtree collision resolves depends on `core.ignoreCase` (git folds
	// the name to clobber the symlink only when it is on): under a case-insensitive filesystem both succeed
	// and replace the symlink; under a case-sensitive one (`core.ignoreCase=false`) both refuse. Assert gta
	// tracks git either way, rather than a fixed outcome.
	assert_eq!(
		switch_try_gta(a.to_str().unwrap()).0,
		switch_try_git(b.to_str().unwrap()).0,
		"exit parity"
	);
	assert_eq!(snapshot(&a), snapshot(&b), "index+worktree parity");
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

/// Exhaustive directory/file-collision truth table (file `p` vs subtree `p/c`) across HEAD/target/index
/// shapes and worktree states, cross-checked cell-by-cell against git 2.55. This is the oracle for git's
/// two-way-merge D/F resolution; any divergence in exit status or the resulting index/worktree fails.
#[test]
fn switch_df_conflict_full_matrix_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// (HEAD `o`, target `n`, index `i`, worktree state). Shapes: pfile=file p, psub=subtree p/c, psube=edited
	// p/c, both=p+p/c, none=absent.
	let specs: &[(&str, &str, &str, &str)] = &[
		("pfile", "psub", "none", "present"),
		("pfile", "psub", "none", "absent"),
		("pfile", "psub", "none", "dirty"),
		("pfile", "psub", "pfile", "present"),
		("pfile", "psub", "pfile", "absent"),
		("pfile", "psub", "pfile", "dirty"),
		("pfile", "psub", "psub", "present"),
		("pfile", "psub", "psub", "absent"),
		("pfile", "psub", "psub", "dirty"),
		("psub", "pfile", "none", "present"),
		("psub", "pfile", "psub", "present"),
		("psub", "pfile", "psub", "absent"),
		("psub", "pfile", "psub", "dirty"),
		("psub", "pfile", "psube", "present"),
		("psub", "pfile", "psube", "absent"),
		("psub", "pfile", "psube", "dirty"),
		("psub", "pfile", "pfile", "present"),
		("psub", "pfile", "pfile", "absent"),
		("psub", "pfile", "pfile", "dirty"),
		("none", "psub", "pfile", "present"),
		("none", "psub", "pfile", "absent"),
		("none", "psub", "pfile", "dirty"),
		("none", "psub", "pfile", "sibling"),
		("none", "pfile", "psub", "present"),
		("none", "pfile", "psub", "absent"),
		("none", "pfile", "psub", "dirty"),
		("pfile", "none", "psub", "present"),
		("pfile", "none", "psub", "absent"),
		("pfile", "none", "psub", "dirty"),
		("psub", "none", "pfile", "present"),
		("psub", "none", "pfile", "absent"),
		("psub", "none", "pfile", "dirty"),
	];
	for &(o, n, i, wt) in specs {
		let label = format!("o={o} n={n} i={i} wt={wt}");
		let a = build_df_case(o, n, i, wt, &label, "gta");
		let b = build_df_case(o, n, i, wt, &label, "git");
		assert_eq!(
			switch_try_gta_to(a.to_str().unwrap(), "other"),
			switch_try_git_to(b.to_str().unwrap(), "other"),
			"{label}: exit parity"
		);
		assert_eq!(snapshot(&a), snapshot(&b), "{label}: index+worktree parity");
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

/// Directory/file edge configs beyond the flat shape matrix (emptied dirs, recreated dirs, different-content
/// staged children), cross-checked cell-by-cell against git 2.55.
#[test]
fn switch_df_conflict_edge_configs_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	type EdgeCase = (&'static str, fn(&str));
	let cases: &[EdgeCase] = &[
		// Staged `p/c` addition whose working file is deleted (empty `p/` remains); target adds file `p`.
		("emptied-dir-target-file", |w| {
			std::fs::write(format!("{w}/a"), b"a").unwrap();
			git(w, &["add", "a"]);
			commit(w, "base");
			git(w, &["switch", "-q", "-c", "other"]);
			std::fs::write(format!("{w}/p"), b"P").unwrap();
			git(w, &["add", "p"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"c").unwrap();
			git(w, &["add", "p/c"]);
			std::fs::remove_file(format!("{w}/p/c")).unwrap();
		}),
		// HEAD file `p`, target deletes it, index stages the same deletion + recreates `p` as an empty dir.
		("recreated-empty-dir-at-deletion", |w| {
			std::fs::write(format!("{w}/p"), b"X").unwrap();
			git(w, &["add", "p"]);
			commit(w, "base");
			git(w, &["switch", "-q", "-c", "other"]);
			git(w, &["rm", "-q", "p"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			git(w, &["rm", "-q", "p"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
		}),
		// Same, but the target replaces `p` with subtree `p/c` (written into the recreated dir).
		("recreated-dir-target-subtree", |w| {
			std::fs::write(format!("{w}/p"), b"X").unwrap();
			git(w, &["add", "p"]);
			commit(w, "base");
			git(w, &["switch", "-q", "-c", "other"]);
			git(w, &["rm", "-q", "p"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"c").unwrap();
			git(w, &["add", "p/c"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			git(w, &["rm", "-q", "p"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
		}),
		// HEAD file `p` removed, target subtree `p/c`, index stages `p/c` with DIFFERENT clean content.
		("removed-ancestor-different-staged-child", |w| {
			std::fs::write(format!("{w}/p"), b"X").unwrap();
			git(w, &["add", "p"]);
			commit(w, "base");
			git(w, &["switch", "-q", "-c", "other"]);
			git(w, &["rm", "-q", "p"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"TGT").unwrap();
			git(w, &["add", "p/c"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			git(w, &["rm", "-q", "p"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"STAGED").unwrap();
			git(w, &["add", "p/c"]);
		}),
	];
	for (name, setup) in cases {
		let build = |tag: &str| -> PathBuf {
			let work = unique_tmp(&format!("gta-dfedge-{name}-{tag}"));
			let w = work.to_str().unwrap();
			git(
				w,
				&["init", "-q", "-b", "main", "--object-format=sha256", "."],
			);
			setup(w);
			work
		};
		let a = build("gta");
		let b = build("git");
		assert_eq!(
			switch_try_gta_to(a.to_str().unwrap(), "other"),
			switch_try_git_to(b.to_str().unwrap(), "other"),
			"{name}: exit parity"
		);
		assert_eq!(snapshot(&a), snapshot(&b), "{name}: index+worktree parity");
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

// Build a repo exercising the file-`p`-vs-subtree-`p/c` D/F conflict: HEAD shape `o`, target branch `other`
// shape `n`, index shape `i`, worktree state `wt`. Uses `update-index --cacheinfo` to craft states (incl.
// case-colliding indexes) git could not otherwise reach.
fn build_df_case(o: &str, n: &str, i: &str, wt: &str, label: &str, tag: &str) -> PathBuf {
	let work = unique_tmp(&format!(
		"gta-dfmx-{}-{tag}",
		label.replace(['=', ' '], "_")
	));
	let w = work.to_str().unwrap();
	git(
		w,
		&["init", "-q", "-b", "main", "--object-format=sha256", "."],
	);
	std::fs::write(work.join("z"), b"z\n").unwrap();
	git(w, &["add", "z"]);
	commit(w, "base");
	df_clear(w);
	df_shape(w, o);
	if o != "none" {
		commit(w, "o");
	}
	git(w, &["switch", "-q", "-c", "other"]);
	df_clear(w);
	df_shape(w, n);
	git(
		w,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"commit",
			"-q",
			"--allow-empty",
			"-m",
			"n",
		],
	);
	git(w, &["switch", "-q", "main"]);
	df_clear(w);
	df_shape(w, i);
	let _ = std::fs::remove_file(work.join("p"));
	let _ = std::fs::remove_dir_all(work.join("p"));
	match wt {
		"present" => match i {
			"pfile" => std::fs::write(work.join("p"), b"A").unwrap(),
			"psub" => {
				std::fs::create_dir(work.join("p")).unwrap();
				std::fs::write(work.join("p/c"), b"C").unwrap();
			}
			"psube" => {
				std::fs::create_dir(work.join("p")).unwrap();
				std::fs::write(work.join("p/c"), b"E").unwrap();
			}
			"both" => std::fs::write(work.join("p"), b"A").unwrap(),
			_ => {}
		},
		"dirty" => match i {
			"pfile" => std::fs::write(work.join("p"), b"DIRTY").unwrap(),
			"psub" | "psube" => {
				std::fs::create_dir(work.join("p")).unwrap();
				std::fs::write(work.join("p/c"), b"DIRTY").unwrap();
			}
			_ => {}
		},
		"sibling" => {
			std::fs::create_dir(work.join("p")).unwrap();
			std::fs::write(work.join("p/q"), b"Q").unwrap();
		}
		_ => {}
	}
	work
}

fn df_clear(w: &str) {
	let _ = Command::new("git")
		.args(["-C", w, "update-index", "--force-remove", "p"])
		.output();
	let listed = Command::new("git")
		.args(["-C", w, "ls-files", "p/"])
		.output()
		.unwrap();
	for line in String::from_utf8_lossy(&listed.stdout).lines() {
		let _ = Command::new("git")
			.args(["-C", w, "update-index", "--force-remove", line])
			.output();
	}
}

fn df_blob(w: &str, content: &str) -> String {
	let out = Command::new("git")
		.args(["-C", w, "hash-object", "-w", "--stdin"])
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::piped())
		.spawn()
		.and_then(|mut ch| {
			use std::io::Write;
			ch.stdin.take().unwrap().write_all(content.as_bytes())?;
			ch.wait_with_output()
		})
		.unwrap();
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn df_shape(w: &str, shape: &str) {
	let add = |path: &str, content: &str| {
		let blob = df_blob(w, content);
		git(
			w,
			&[
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("100644,{blob},{path}"),
			],
		);
	};
	match shape {
		"pfile" => add("p", "A"),
		"psub" => add("p/c", "C"),
		"psube" => add("p/c", "E"),
		"both" => {
			add("p", "A");
			add("p/c", "C");
		}
		_ => {}
	}
}

/// git resolves directory/file collisions across a switch asymmetrically; each scenario builds identical
/// `gta`/`git` repos, switches, and asserts exit + index + worktree parity (probed vs git 2.55).
#[test]
fn switch_directory_file_collision_matrix_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	type DfScenario = (&'static str, fn(&str));
	let scenarios: &[DfScenario] = &[
		// Target adds a nested child over a staged-only parent file (present, clean) → target wins.
		("target-child-vs-staged-parent-file", |w| {
			git(w, &["switch", "-q", "-c", "other"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"c\n").unwrap();
			git(w, &["add", "p/c"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			std::fs::write(format!("{w}/p"), b"P\n").unwrap();
			git(w, &["add", "p"]);
		}),
		// Same, but the staged parent file's working copy is deleted (AD) → target still wins.
		("target-child-vs-staged-parent-file-deleted", |w| {
			git(w, &["switch", "-q", "-c", "other"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"c\n").unwrap();
			git(w, &["add", "p/c"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			std::fs::write(format!("{w}/p"), b"P\n").unwrap();
			git(w, &["add", "p"]);
			std::fs::remove_file(format!("{w}/p")).unwrap();
		}),
		// HEAD tracks p/c; target replaces with file p; index has a staged edit to p/c → incoming file wins.
		("target-file-vs-staged-edit-subtree", |w| {
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"c\n").unwrap();
			git(w, &["add", "p/c"]);
			commit(w, "add-subtree");
			git(w, &["switch", "-q", "-c", "other"]);
			git(w, &["rm", "-q", "-r", "p"]);
			std::fs::write(format!("{w}/p"), b"P\n").unwrap();
			git(w, &["add", "p"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			std::fs::write(format!("{w}/p/c"), b"EDIT\n").unwrap();
			git(w, &["add", "p/c"]);
		}),
		// `D p` (HEAD file) + staged `A p/c` + target deletes p → git switches and drops the colliding p/c.
		("removed-head-file-drops-staged-child", |w| {
			std::fs::write(format!("{w}/p"), b"X\n").unwrap();
			git(w, &["add", "p"]);
			commit(w, "add-file");
			git(w, &["switch", "-q", "-c", "other"]);
			git(w, &["rm", "-q", "p"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			git(w, &["rm", "-q", "p"]);
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"c\n").unwrap();
			git(w, &["add", "p/c"]);
		}),
		// Staged deletion of HEAD's p/c + an untracked file p (a blocking non-dir ancestor) → git refuses.
		("staged-del-subtree-blocked-by-untracked-ancestor", |w| {
			std::fs::create_dir(format!("{w}/p")).unwrap();
			std::fs::write(format!("{w}/p/c"), b"c\n").unwrap();
			git(w, &["add", "p/c"]);
			commit(w, "add-subtree");
			git(w, &["switch", "-q", "-c", "other"]);
			git(w, &["rm", "-q", "-r", "p"]);
			commit(w, "t");
			git(w, &["switch", "-q", "main"]);
			git(w, &["rm", "-q", "p/c"]);
			std::fs::write(format!("{w}/p"), b"U\n").unwrap();
		}),
	];
	for (name, setup) in scenarios {
		let build = |tag: &str| -> PathBuf {
			let work = unique_tmp(&format!("gta-switch-dfmatrix-{name}-{tag}"));
			let w = work.to_str().unwrap();
			git(
				w,
				&["init", "-q", "-b", "main", "--object-format=sha256", "."],
			);
			std::fs::write(work.join("a"), b"a\n").unwrap();
			git(w, &["add", "a"]);
			commit(w, "base");
			setup(w);
			work
		};
		let a = build("gta");
		let b = build("git");
		assert_eq!(
			switch_try_gta_to(a.to_str().unwrap(), "other"),
			switch_try_git_to(b.to_str().unwrap(), "other"),
			"{name}: exit parity"
		);
		assert_eq!(snapshot(&a), snapshot(&b), "{name}: index+worktree parity");
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

#[test]
fn switch_checks_out_target_over_head_tracked_df_child_like_git() {
	// HEAD tracks `thing/child`; the target replaces that subtree with a file `thing`; the user unstaged-
	// deletes the child. The staged/HEAD entry is an ordinary tree-diff removal (not staged-only), so the
	// target `thing` must be checked out — not skipped, leaving an empty slot.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-head-df-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::create_dir(work.join("thing")).unwrap();
		std::fs::write(work.join("thing/child"), b"c\n").unwrap();
		git(w, &["add", "."]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		git(w, &["rm", "-q", "-r", "thing"]);
		std::fs::write(work.join("thing"), b"T\n").unwrap();
		git(w, &["add", "thing"]);
		commit(w, "t");
		git(w, &["switch", "-q", "main"]);
		std::fs::remove_dir_all(work.join("thing")).unwrap();
		work
	};
	let a = build("gta");
	let b = build("git");
	assert!(
		switch_try_gta(a.to_str().unwrap()).0,
		"the switch must succeed"
	);
	assert!(
		switch_try_git(b.to_str().unwrap()).0,
		"sanity: git succeeds"
	);
	assert_eq!(snapshot(&a), snapshot(&b), "index+worktree parity");
	assert_eq!(
		git(a.to_str().unwrap(), &["ls-files"]).trim(),
		"thing",
		"target must be checked out"
	);
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

#[test]
fn switch_accepts_folded_tracked_rename_satisfied_like_git() {
	// HEAD tracks `Foo`; the index and target both hold the case-only rename `foo`. On a case-insensitive
	// filesystem `lstat("Foo")` reaches `foo`'s tracked inode, which must not be treated as a recreated
	// untracked file blocking the (already-satisfied) switch — git accepts it.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-folded-sat-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "core.ignoreCase", "true"]);
		std::fs::write(work.join("Foo"), b"X\n").unwrap();
		git(w, &["add", "Foo"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		git(w, &["mv", "Foo", "foo"]);
		commit(w, "t");
		git(w, &["switch", "-q", "main"]);
		git(w, &["mv", "Foo", "foo"]); // stage the same case-rename locally
		work
	};
	let a = build("gta");
	let b = build("git");
	assert!(
		switch_try_gta(a.to_str().unwrap()).0,
		"gta must accept the satisfied folded rename"
	);
	assert!(
		switch_try_git(b.to_str().unwrap()).0,
		"sanity: git succeeds"
	);
	assert_eq!(snapshot(&a), snapshot(&b), "index+worktree parity");
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

#[test]
fn switch_refuses_colliding_stage0_recase_deterministically() {
	// A case-colliding stage-0 index (`Foo` and `foo`, hand-crafted) whose target recases the fold-key to a
	// third spelling `FOO`: git refuses, and gta must refuse DETERMINISTICALLY (repeated across runs), not
	// depend on which colliding entry a folded lookup happened to retain.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	for attempt in 0..6 {
		let work = unique_tmp(&format!("gta-switch-collide-recase-{attempt}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "core.ignoreCase", "true"]);
		std::fs::write(work.join("z"), b"z\n").unwrap();
		git(w, &["add", "z"]);
		commit(w, "base");
		for spec in ["Foo", "foo"] {
			std::fs::write(work.join("blobsrc"), format!("{spec}-content\n")).unwrap();
			let blob = git(w, &["hash-object", "-w", "blobsrc"]).trim().to_owned();
			git(
				w,
				&[
					"-c",
					"core.ignoreCase=false",
					"update-index",
					"--add",
					"--cacheinfo",
					&format!("100644,{blob},{spec}"),
				],
			);
		}
		std::fs::remove_file(work.join("blobsrc")).unwrap();
		commit(w, "colliding");
		git(w, &["switch", "-q", "-c", "up"]);
		git(
			w,
			&[
				"-c",
				"core.ignoreCase=false",
				"rm",
				"-q",
				"--cached",
				"Foo",
				"foo",
			],
		);
		std::fs::write(work.join("blobsrc"), b"Z\n").unwrap();
		let blobz = git(w, &["hash-object", "-w", "blobsrc"]).trim().to_owned();
		std::fs::remove_file(work.join("blobsrc")).unwrap();
		git(
			w,
			&[
				"-c",
				"core.ignoreCase=false",
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("100644,{blobz},FOO"),
			],
		);
		commit(w, "recase");
		git(w, &["switch", "-q", "-f", "main"]);
		git(w, &["reset", "-q", "--hard", "main"]);
		std::fs::write(work.join("Foo"), b"Foo-content\n").unwrap();
		assert!(
			!switch_try_gta_to(w, "up"),
			"attempt {attempt}: a colliding-index recase must refuse deterministically"
		);
		std::fs::remove_dir_all(&work).ok();
	}
}

#[test]
fn switch_preserves_staged_df_entry_with_unstaged_deletion_like_git() {
	// A staged-only `thing/child` whose working file is then deleted (`AD`), with a target that adds a file
	// `thing`: the on-disk collision is gone, so git preserves the staged entry and its deletion rather than
	// materialising `thing` — gta must not silently discard the staged blob.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-df-unstaged-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		std::fs::write(work.join("a"), b"a\n").unwrap();
		git(w, &["add", "a"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		std::fs::write(work.join("thing"), b"T\n").unwrap();
		git(w, &["add", "thing"]);
		commit(w, "t");
		git(w, &["switch", "-q", "main"]);
		std::fs::create_dir(work.join("thing")).unwrap();
		std::fs::write(work.join("thing/child"), b"c\n").unwrap();
		git(w, &["add", "thing/child"]);
		std::fs::remove_dir_all(work.join("thing")).unwrap(); // unstaged deletion
		work
	};
	let a = build("gta");
	let b = build("git");
	assert_eq!(
		switch_try_gta(a.to_str().unwrap()).0,
		switch_try_git(b.to_str().unwrap()).0,
		"exit parity"
	);
	assert_eq!(
		snapshot(&a),
		snapshot(&b),
		"staged D/F entry must be preserved, not discarded"
	);
	assert_eq!(
		git(a.to_str().unwrap(), &["ls-files"]).trim(),
		"a\nthing/child",
		"the staged thing/child entry survives"
	);
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

#[test]
fn switch_carries_case_colliding_addition_against_new_target_like_git() {
	// Under `core.ignoreCase`, a staged addition `foo` (HEAD lacks the fold-key) plus a target that ADDS a
	// different-cased `Foo`: these are two independent additions, not a conflict. git keeps both index entries
	// and materialises `Foo`; gta must not reject the switch as an overwrite.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-collide-newtarget-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "core.ignoreCase", "true"]);
		std::fs::write(work.join("a"), b"a\n").unwrap();
		git(w, &["add", "a"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		std::fs::write(work.join("Foo"), b"FOO\n").unwrap();
		git(w, &["add", "Foo"]);
		commit(w, "t");
		git(w, &["switch", "-q", "main"]);
		std::fs::write(work.join("blobsrc"), b"bar\n").unwrap();
		let blob = git(w, &["hash-object", "-w", "blobsrc"]).trim().to_owned();
		std::fs::remove_file(work.join("blobsrc")).unwrap();
		git(
			w,
			&[
				"-c",
				"core.ignoreCase=false",
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("100644,{blob},foo"),
			],
		);
		work
	};
	let a = build("gta");
	let b = build("git");
	assert!(
		switch_try_gta(a.to_str().unwrap()).0,
		"gta must accept the colliding addition"
	);
	assert!(
		switch_try_git(b.to_str().unwrap()).0,
		"sanity: git succeeds"
	);
	assert_eq!(snapshot(&a), snapshot(&b), "index+worktree parity");
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

#[test]
fn switch_carries_colliding_staged_addition_like_git() {
	// Under `core.ignoreCase`, a tracked `Foo` plus a staged addition `foo` (a colliding addition, not a
	// rename): switching to a branch that deletes `Foo` must remove `Foo` and carry the staged `foo`, not
	// preserve both — the folded entry is only a staged recase when HEAD's own spelling has left the index.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-collide-add-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "core.ignoreCase", "true"]);
		std::fs::write(work.join("Foo"), b"X\n").unwrap();
		git(w, &["add", "Foo"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		git(w, &["rm", "-q", "Foo"]);
		commit(w, "t");
		git(w, &["switch", "-q", "main"]);
		std::fs::write(work.join("blobsrc"), b"Y\n").unwrap();
		let blob = git(w, &["hash-object", "-w", "blobsrc"]).trim().to_owned();
		std::fs::remove_file(work.join("blobsrc")).unwrap();
		// Stage a colliding `foo` alongside the tracked `Foo` (added case-sensitively).
		git(
			w,
			&[
				"-c",
				"core.ignoreCase=false",
				"update-index",
				"--add",
				"--cacheinfo",
				&format!("100644,{blob},foo"),
			],
		);
		work
	};
	let a = build("gta");
	let b = build("git");
	assert!(
		switch_try_gta(a.to_str().unwrap()).0,
		"gta switch must succeed"
	);
	assert!(
		switch_try_git(b.to_str().unwrap()).0,
		"sanity: git succeeds"
	);
	assert_eq!(snapshot(&a), snapshot(&b), "index+worktree parity");
	assert_eq!(git(a.to_str().unwrap(), &["ls-files"]).trim(), "foo");
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

/// git re-applies the sparse patterns to the whole switched index, but `merge_apply` only diffs the two
/// trees — so an out-of-cone index entry that the diff does not touch must still be reconciled to match
/// git. Covers: a `git add --sparse` path unchanged between branches (clean → removed + bit set, blob
/// preserved; dirty → left with bit clear, git's "left despite sparse patterns"); a clean file recreated
/// at an already-omitted path (removed); and a `--sparse` path the target also changes to the same blob,
/// i.e. satisfied by the switch (removed + bit set). Each must match git exactly.
#[test]
fn switch_reapplies_sparsity_to_carried_staged_paths_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	// Each setup runs in a repo already committed at `base` (in/a + out/p="p"); it arranges the out-of-cone
	// state and the `other` branch, and the switch target is always `other`.
	type Setup = fn(&str);
	let scenarios: &[(&str, Setup)] = &[
		("clean-carried", |w| {
			git(w, &["branch", "other"]); // IDENTICAL out/p on both branches
			git(w, &["sparse-checkout", "set", "--no-cone", "in"]);
			std::fs::create_dir_all(format!("{w}/out")).unwrap();
			std::fs::write(format!("{w}/out/p"), b"pEDIT").unwrap();
			git(w, &["add", "--sparse", "out/p"]);
		}),
		("dirty-carried", |w| {
			git(w, &["branch", "other"]);
			git(w, &["sparse-checkout", "set", "--no-cone", "in"]);
			std::fs::create_dir_all(format!("{w}/out")).unwrap();
			std::fs::write(format!("{w}/out/p"), b"pEDIT").unwrap();
			git(w, &["add", "--sparse", "out/p"]);
			std::fs::write(format!("{w}/out/p"), b"pDIRTY").unwrap(); // diverge from the staged blob
		}),
		("recreated-at-omitted", |w| {
			git(w, &["branch", "other"]);
			git(w, &["sparse-checkout", "set", "--no-cone", "in"]); // out/p omitted, file removed, bit set
			std::fs::create_dir_all(format!("{w}/out")).unwrap();
			std::fs::write(format!("{w}/out/p"), b"p").unwrap(); // recreate the clean file at the omitted path
		}),
		("satisfied-modified", |w| {
			git(w, &["switch", "-q", "-c", "other"]);
			std::fs::write(format!("{w}/out/p"), b"pEDIT").unwrap();
			git(w, &["add", "out/p"]);
			commit(w, "mod"); // `other` has out/p="pEDIT"
			git(w, &["switch", "-q", "main"]);
			git(w, &["sparse-checkout", "set", "--no-cone", "in"]);
			std::fs::create_dir_all(format!("{w}/out")).unwrap();
			std::fs::write(format!("{w}/out/p"), b"pEDIT").unwrap();
			git(w, &["add", "--sparse", "out/p"]); // staged blob == target → satisfied by the switch
		}),
	];
	for (name, setup) in scenarios {
		let build = |tag: &str| -> PathBuf {
			let work = unique_tmp(&format!("gta-switch-sparse-{tag}"));
			let w = work.to_str().unwrap();
			git(
				w,
				&["init", "-q", "-b", "main", "--object-format=sha256", "."],
			);
			std::fs::create_dir(work.join("in")).unwrap();
			std::fs::create_dir(work.join("out")).unwrap();
			std::fs::write(work.join("in/a"), b"a").unwrap();
			std::fs::write(work.join("out/p"), b"p").unwrap();
			git(w, &["add", "-A"]);
			commit(w, "base");
			setup(w);
			work
		};
		let a = build("gta");
		let b = build("git");
		assert_eq!(
			switch_try_gta_to(a.to_str().unwrap(), "other"),
			switch_try_git_to(b.to_str().unwrap(), "other"),
			"{name}: switch exit parity"
		);
		let observe = |work: &PathBuf| -> String {
			let w = work.to_str().unwrap();
			format!(
				"present={} content={:?}\nls-files-t: {}\nstaged: {}",
				work.join("out/p").exists(),
				std::fs::read_to_string(work.join("out/p")).unwrap_or_default(),
				git(w, &["ls-files", "-t", "out/p"]).trim(),
				git(w, &["rev-parse", ":out/p"]).trim(),
			)
		};
		assert_eq!(
			observe(&a),
			observe(&b),
			"{name}: sparse reconciliation parity (file, skip-worktree bit, staged blob)"
		);
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

/// `core.ignoreCase`: HEAD tracks `P` (a file); the target replaces it with a subtree `p/c`. On a
/// case-insensitive worktree the `p` slot IS the tracked `P`, so the D/F untracked-overwrite preflight must
/// recognise it as tracked (by fold-key) rather than an untracked ancestor and abort. git switches and
/// materialises `p/c` (probed vs git 2.55); gta must too.
#[test]
fn switch_replaces_case_variant_tracked_file_with_subtree_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-foldDF-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "core.ignoreCase", "true"]);
		std::fs::write(work.join("P"), b"PVAL").unwrap();
		git(w, &["add", "P"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		git(w, &["rm", "-q", "P"]);
		std::fs::create_dir(work.join("p")).unwrap();
		std::fs::write(work.join("p/c"), b"CVAL").unwrap();
		git(w, &["add", "p/c"]);
		commit(w, "sub"); // target replaces the file `P` with the subtree `p/c`
		git(w, &["switch", "-q", "main"]);
		work
	};
	let a = build("gta");
	let b = build("git");
	assert_eq!(
		switch_try_gta_to(a.to_str().unwrap(), "other"),
		switch_try_git_to(b.to_str().unwrap(), "other"),
		"switch exit parity"
	);
	// git-LEVEL state (a case-insensitive fs makes the raw dirent case an artifact).
	let observe = |work: &PathBuf| -> String {
		let w = work.to_str().unwrap();
		format!(
			"ls-files:\n{}\nstatus:\n{}",
			git(w, &["ls-files"]),
			git(w, &["status", "--porcelain"]),
		)
	};
	assert_eq!(observe(&a), observe(&b), "index+status parity");
	assert!(
		a.join("p/c").is_file(),
		"the target subtree must materialise"
	);
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

/// A tracked FILE replaced on disk by a directory (empty, or holding only ignored files) whose target
/// MODIFIES it: git refuses (the tracked file has an unstaged deletion), so gta must too — clearing the
/// directory to materialise the target would silently destroy its contents, including IGNORED files
/// (probed vs git 2.55).
#[test]
fn switch_refuses_tracked_file_replaced_by_directory_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	for (name, ignored) in [("with-ignored-file", true), ("empty-dir", false)] {
		let build = |tag: &str| -> PathBuf {
			let work = unique_tmp(&format!("gta-switch-file2dir-{tag}"));
			let w = work.to_str().unwrap();
			git(
				w,
				&["init", "-q", "-b", "main", "--object-format=sha256", "."],
			);
			std::fs::write(work.join("p"), b"orig").unwrap();
			std::fs::write(work.join(".gitignore"), b"p/ign\n").unwrap();
			git(w, &["add", "p", ".gitignore"]);
			commit(w, "base");
			git(w, &["switch", "-q", "-c", "other"]);
			std::fs::write(work.join("p"), b"MODIFIED").unwrap();
			git(w, &["add", "p"]);
			commit(w, "mod");
			git(w, &["switch", "-q", "main"]);
			std::fs::remove_file(work.join("p")).unwrap();
			std::fs::create_dir(work.join("p")).unwrap(); // tracked file `p` replaced by a directory
			if ignored {
				std::fs::write(work.join("p/ign"), b"secret").unwrap(); // ignored content that must survive
			}
			work
		};
		let a = build("gta");
		let b = build("git");
		assert!(
			!switch_try_gta_to(a.to_str().unwrap(), "other"),
			"{name}: gta must refuse (unstaged file→dir deletion)"
		);
		assert!(
			!switch_try_git_to(b.to_str().unwrap(), "other"),
			"{name}: sanity — git refuses"
		);
		assert!(a.join("p").is_dir(), "{name}: p must stay a directory");
		if ignored {
			assert_eq!(
				std::fs::read(a.join("p/ign")).unwrap(),
				b"secret",
				"{name}: the ignored file must not be destroyed"
			);
		}
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

/// `core.ignoreCase` fold cases in the two-tree switch, each probed vs git 2.55:
/// - **staged-addition-folded**: HEAD lacks the fold-key, the target adds `Foo`, and a normal `git add foo`
///   is staged — git switches and keeps both (`AM foo`); gta must not treat the folded worktree alias as
///   untracked and refuse.
/// - **staged-deletion-across-recase**: HEAD tracks `Foo`, `git rm Foo` is staged, the target renames it to
///   `foo` — git switches and checks out `foo`; gta must not call the staged deletion a conflict.
/// - **hand-crafted-colliding-dirty**: a hand-crafted case-colliding index (staged `foo` while HEAD has
///   `Foo`) whose target deletes the fold-key with a DIRTY working alias — git refuses; gta must too, rather
///   than blindly exempting the folded alias.
#[test]
fn switch_ignorecase_fold_edge_cases_like_git() {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	type Setup = fn(&str);
	let scenarios: &[(&str, Setup)] = &[
		("staged-addition-folded", |w| {
			git(w, &["rm", "-q", "Foo"]);
			commit(w, "rm"); // HEAD loses the fold-key
			git(w, &["switch", "-q", "-c", "other"]);
			std::fs::write(format!("{w}/Foo"), b"FOO").unwrap();
			git(w, &["add", "Foo"]);
			commit(w, "addFoo");
			git(w, &["switch", "-q", "main"]);
			std::fs::write(format!("{w}/foo"), b"foo").unwrap();
			git(w, &["add", "foo"]); // normal staged addition of the other case
		}),
		("staged-deletion-across-recase", |w| {
			git(w, &["switch", "-q", "-c", "other"]);
			git(w, &["rm", "-q", "Foo"]);
			std::fs::write(format!("{w}/foo"), b"FOO").unwrap();
			git(w, &["add", "foo"]);
			commit(w, "rename"); // target renames Foo -> foo
			git(w, &["switch", "-q", "main"]);
			git(w, &["rm", "-q", "Foo"]); // staged deletion of the source
		}),
		("hand-crafted-colliding-dirty", |w| {
			let blob = git(w, &["rev-parse", "HEAD:Foo"]).trim().to_owned();
			git(w, &["switch", "-q", "-c", "other"]);
			git(w, &["rm", "-q", "Foo"]);
			commit(w, "delFoo"); // target deletes the fold-key
			git(w, &["switch", "-q", "main"]);
			// Craft the index to hold `foo` while HEAD holds `Foo` (git folds it away under normal use).
			git(
				w,
				&[
					"-c",
					"core.ignoreCase=false",
					"update-index",
					"--force-remove",
					"Foo",
				],
			);
			git(
				w,
				&[
					"-c",
					"core.ignoreCase=false",
					"update-index",
					"--add",
					"--cacheinfo",
					&format!("100644,{blob},foo"),
				],
			);
			std::fs::write(format!("{w}/foo"), b"DIRTY").unwrap(); // dirty alias blocks the switch
		}),
	];
	let insensitive = case_insensitive_fs();
	for (name, setup) in scenarios {
		// A hand-crafted `Foo`+`foo` colliding index is a case-insensitive-filesystem artifact; on a
		// case-sensitive filesystem the two are distinct files and the scenario tests nothing.
		if *name == "hand-crafted-colliding-dirty" && !insensitive {
			continue;
		}
		let build = |tag: &str| -> PathBuf {
			let work = unique_tmp(&format!("gta-switch-fold-{tag}"));
			let w = work.to_str().unwrap();
			git(
				w,
				&["init", "-q", "-b", "main", "--object-format=sha256", "."],
			);
			git(w, &["config", "core.ignoreCase", "true"]);
			std::fs::write(work.join("Foo"), b"FOO").unwrap();
			git(w, &["add", "Foo"]);
			commit(w, "base");
			setup(w);
			work
		};
		let a = build("gta");
		let b = build("git");
		assert_eq!(
			switch_try_gta_to(a.to_str().unwrap(), "other"),
			switch_try_git_to(b.to_str().unwrap(), "other"),
			"{name}: switch exit parity"
		);
		// Compare the git-LEVEL state (index entries + porcelain status), not `snapshot`'s raw on-disk
		// filenames: on a case-insensitive filesystem the stored dirent case (`Foo` vs `foo`) is an fs
		// artifact that can differ between two runs even when the tracked state is identical.
		let observe = |work: &PathBuf| -> String {
			let w = work.to_str().unwrap();
			format!(
				"ls-files-s:\n{}\nstatus:\n{}",
				git(w, &["ls-files", "-s"]),
				git(w, &["status", "--porcelain"]),
			)
		};
		assert_eq!(observe(&a), observe(&b), "{name}: index+status parity");
		std::fs::remove_dir_all(&a).ok();
		std::fs::remove_dir_all(&b).ok();
	}
}

fn switch_try_gta_to(dir: &str, branch: &str) -> bool {
	assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir, "switch", branch])
		.output()
		.expect("run gta switch")
		.status
		.success()
}

fn switch_try_git_to(dir: &str, branch: &str) -> bool {
	Command::new("git")
		.args(["-C", dir, "switch", branch])
		.output()
		.expect("run git switch")
		.status
		.success()
}

#[test]
fn switch_applies_branch_case_rename_like_git() {
	// Under `core.ignoreCase`, switching to a branch that recases `Foo`->`foo` (same blob) must apply the
	// rename — the folded target spelling must not be mistaken for an already-satisfied path and skipped,
	// which would delete the file from the index.
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return;
	}
	let build = |tag: &str| -> PathBuf {
		let work = unique_tmp(&format!("gta-switch-caserename-{tag}"));
		let w = work.to_str().unwrap();
		git(
			w,
			&["init", "-q", "-b", "main", "--object-format=sha256", "."],
		);
		git(w, &["config", "core.ignoreCase", "true"]);
		std::fs::write(work.join("Foo"), b"X\n").unwrap();
		git(w, &["add", "Foo"]);
		commit(w, "base");
		git(w, &["switch", "-q", "-c", "other"]);
		git(w, &["mv", "Foo", "foo"]);
		commit(w, "recase");
		git(w, &["switch", "-q", "main"]);
		work
	};
	let a = build("gta");
	let b = build("git");
	assert!(
		switch_try_gta(a.to_str().unwrap()).0,
		"gta case-rename switch must succeed"
	);
	assert!(
		switch_try_git(b.to_str().unwrap()).0,
		"sanity: git succeeds"
	);
	assert_eq!(
		snapshot(&a),
		snapshot(&b),
		"case-rename index+worktree parity"
	);
	assert_eq!(git(a.to_str().unwrap(), &["ls-files"]).trim(), "foo");
	std::fs::remove_dir_all(&a).ok();
	std::fs::remove_dir_all(&b).ok();
}

// Scenario builders for `switch_two_way_merges_staged_changes_like_git` — module-level so the scenario
// closures that call them stay non-capturing (coercible to `fn(&str)`).
fn other_same(w: &str) {
	git(w, &["branch", "other"]);
}
fn other_mod(w: &str) {
	git(w, &["switch", "-q", "-c", "other"]);
	std::fs::write(format!("{w}/foo"), b"fooT\n").unwrap();
	git(w, &["add", "foo"]);
	commit(w, "t");
	git(w, &["switch", "-q", "main"]);
}
fn other_del(w: &str) {
	git(w, &["switch", "-q", "-c", "other"]);
	git(w, &["rm", "-q", "foo"]);
	commit(w, "t");
	git(w, &["switch", "-q", "main"]);
}
fn other_add(w: &str) {
	git(w, &["switch", "-q", "-c", "other"]);
	std::fs::write(format!("{w}/bar"), b"OTH\n").unwrap();
	git(w, &["add", "bar"]);
	commit(w, "t");
	git(w, &["switch", "-q", "main"]);
}

fn switch_try_gta(dir: &str) -> (bool, ()) {
	let out = assert_cmd::Command::cargo_bin("gta")
		.unwrap()
		.args(["-C", dir, "switch", "other"])
		.output()
		.expect("run gta switch");
	(out.status.success(), ())
}

fn switch_try_git(dir: &str) -> (bool, ()) {
	let out = Command::new("git")
		.args(["-C", dir, "switch", "other"])
		.output()
		.expect("run git switch");
	(out.status.success(), ())
}

/// A comparable snapshot of the index and working tree: staged entries (with oids) and the porcelain
/// status, plus every regular file's content.
fn snapshot(work: &std::path::Path) -> String {
	let w = work.to_str().unwrap();
	let mut files: Vec<String> = std::fs::read_dir(work)
		.unwrap()
		.filter_map(|e| e.ok())
		.filter(|e| e.file_name() != ".git" && e.path().is_file())
		.map(|e| {
			let name = e.file_name().into_string().unwrap();
			let content = std::fs::read_to_string(e.path()).unwrap_or_default();
			format!("{name}={content:?}")
		})
		.collect();
	files.sort();
	format!(
		"ls-files-s:\n{}\nstatus:\n{}\nfiles:\n{}",
		git(w, &["ls-files", "-s"]),
		git(w, &["status", "--porcelain"]),
		files.join("\n"),
	)
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
	let dir = std::env::temp_dir().join(format!("gitana-{tag}-{}-{seq}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// Whether the temp filesystem folds case (macOS APFS default, Windows). A scenario that hand-crafts a
/// `Foo`+`foo` case-colliding index only exists on such a filesystem; on a case-sensitive one (Linux CI)
/// the two are distinct real files and the scenario tests nothing, so it is skipped there.
fn case_insensitive_fs() -> bool {
	let dir = unique_tmp("gta-case-probe");
	std::fs::write(dir.join("CaseProbe"), b"x").unwrap();
	let insensitive = dir.join("caseprobe").exists();
	let _ = std::fs::remove_dir_all(&dir);
	insensitive
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("gta-probe");
		let ok = Command::new("git")
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
