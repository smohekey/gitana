//! Enumeration + discovery contexts (bare, from inside a linked worktree), over SHA-1 and SHA-256.
#![cfg(unix)]

mod common;

use common::*;
use gitana_linked_worktree::{HeadKind, LockState, RepositoryId, WorktreeRole, enumerate};

#[tokio::test]
async fn enumerates_primary_and_linked_worktrees_with_their_facts() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("enum-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();

		let attached = base.join("attached");
		let detached = base.join("detached");
		let locked = base.join("locked");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			attached.to_str().unwrap(),
		]);
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"--detach",
			detached.to_str().unwrap(),
		]);
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"other",
			locked.to_str().unwrap(),
		]);
		git(&[
			"-C",
			w,
			"worktree",
			"lock",
			"--reason",
			"busy",
			locked.to_str().unwrap(),
		]);

		let listing = enumerate(&rid_at(&work)).await.unwrap();

		// Primary first, not bare, on a symbolic branch.
		assert!(matches!(
			listing.entries[0].role,
			WorktreeRole::Primary { bare: false }
		));
		assert_eq!(listing.entries[0].head, Some(HeadKind::Symbolic));
		assert_eq!(listing.entries.len(), 4);

		let find = |path: &std::path::Path| {
			listing
				.entries
				.iter()
				.find(|e| e.path == canonical(path))
				.unwrap_or_else(|| panic!("missing entry for {}", path.display()))
		};

		let a = find(&attached);
		assert_eq!(a.branch.as_deref(), Some("refs/heads/feature"));
		assert_eq!(a.head, Some(HeadKind::Symbolic));
		assert!(a.object.is_some());
		assert!(!a.checkout_missing);
		assert_eq!(a.lock, LockState::Unlocked);

		let d = find(&detached);
		assert_eq!(d.head, Some(HeadKind::Detached));
		assert_eq!(d.branch, None);
		assert!(d.object.is_some());

		let l = find(&locked);
		assert_eq!(
			l.lock,
			LockState::Locked {
				reason: Some("busy".to_owned())
			}
		);

		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn enumerates_a_worktree_whose_head_is_a_symbolic_branch() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("enum-symref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		let feat = git(&["-C", w, "rev-parse", "feature"]).trim().to_owned();
		// Make `refs/heads/alias` a symbolic ref to feature, and point the worktree's HEAD at it.
		git(&[
			"-C",
			w,
			"symbolic-ref",
			"refs/heads/alias",
			"refs/heads/feature",
		]);
		std::fs::write(
			work.join(".git/worktrees/wt/HEAD"),
			b"ref: refs/heads/alias\n",
		)
		.unwrap();

		// Enumeration must follow the symbolic branch to its *terminal* (git's worktree list reports
		// `feature`, not `alias`, for HEAD → alias → feature) and resolve its object.
		let porcelain = git(&["-C", w, "worktree", "list", "--porcelain"]);
		assert!(
			porcelain.contains("branch refs/heads/feature"),
			"git reports the terminal branch: {porcelain}"
		);
		let listing = enumerate(&rid_at(&work)).await.unwrap();
		let entry = listing
			.entries
			.iter()
			.find(|e| matches!(e.role, WorktreeRole::Linked { .. }))
			.expect("linked worktree");
		assert_eq!(
			entry.branch.as_deref(),
			Some("refs/heads/feature"),
			"reports the terminal branch"
		);
		assert_eq!(entry.head, Some(HeadKind::Symbolic));
		assert_eq!(entry.object.as_ref().map(|o| o.to_hex()), Some(feat));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_legacy_symlink_head_is_reported_as_a_symbolic_branch() {
	// A `.git/HEAD` that is a *symlink* to a branch (the historical symref form) is symbolic — git reports
	// the branch, not a detached object. Reading it must follow the symlink's *target* as the ref name,
	// never dereference it to the branch's object id.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symlink-head-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let branch = git(&["-C", w, "rev-parse", "--abbrev-ref", "HEAD"])
			.trim()
			.to_owned();
		let obj = git(&["-C", w, "rev-parse", "HEAD"]).trim().to_owned();

		// Replace the regular-file HEAD with a legacy symlink to the same branch.
		let head = work.join(".git/HEAD");
		std::fs::remove_file(&head).unwrap();
		std::os::unix::fs::symlink(format!("refs/heads/{branch}"), &head).unwrap();
		// Oracle: git still treats it as symbolic.
		let porcelain = git(&["-C", w, "worktree", "list", "--porcelain"]);
		assert!(
			porcelain.contains(&format!("branch refs/heads/{branch}")),
			"git reports the symbolic branch for a symlink HEAD: {porcelain}"
		);

		let listing = enumerate(&rid_at(&work)).await.unwrap();
		let primary = &listing.entries[0];
		assert!(matches!(
			primary.role,
			WorktreeRole::Primary { bare: false }
		));
		assert_eq!(primary.head, Some(HeadKind::Symbolic));
		assert_eq!(
			primary.branch.as_deref(),
			Some(&*format!("refs/heads/{branch}"))
		);
		assert_eq!(
			primary.object.as_ref().map(|o| o.to_hex()),
			Some(obj),
			"the symbolic HEAD resolves to the branch's object, not a detached read of the link"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_legacy_symlink_symref_branch_resolves_its_object() {
	// A branch that is a legacy *symlink* symref (`refs/heads/alias -> refs/heads/feature`, a `refs/`-
	// prefixed target) is symbolic to git. Enumeration must report the terminal branch AND resolve the
	// object through that terminal — not follow the broken filesystem symlink and report an unborn HEAD.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("enum-symlink-symref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		let feat = git(&["-C", w, "rev-parse", "feature"]).trim().to_owned();
		// `refs/heads/alias` as a *symlink* to `refs/heads/feature`, and the worktree's HEAD points at it.
		std::os::unix::fs::symlink("refs/heads/feature", work.join(".git/refs/heads/alias")).unwrap();
		std::fs::write(
			work.join(".git/worktrees/wt/HEAD"),
			b"ref: refs/heads/alias\n",
		)
		.unwrap();
		// Oracle: git resolves it to feature's commit.
		let porcelain = git(&["-C", w, "worktree", "list", "--porcelain"]);
		assert!(
			porcelain.contains("branch refs/heads/feature")
				&& porcelain.contains(&format!("HEAD {feat}")),
			"git resolves the symlink symref: {porcelain}"
		);

		let listing = enumerate(&rid_at(&work)).await.unwrap();
		let entry = listing
			.entries
			.iter()
			.find(|e| matches!(e.role, WorktreeRole::Linked { .. }))
			.expect("linked worktree");
		assert_eq!(entry.branch.as_deref(), Some("refs/heads/feature"));
		assert_eq!(entry.head, Some(HeadKind::Symbolic));
		assert_eq!(
			entry.object.as_ref().map(|o| o.to_hex()),
			Some(feat),
			"the object resolves through the terminal ref, not the broken symlink"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn an_explicit_symlink_alias_to_git_is_normalized() {
	// `at_common_dir` given a symlink alias to an ordinary `.git` must resolve it, as git does — otherwise
	// layout inference from the basename would mark it bare and report the alias as the primary path.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("enum-alias-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		// An absolute symlink whose basename is NOT `.git`, pointing at the real git dir.
		let alias = base.join("meta-link");
		std::os::unix::fs::symlink(work.join(".git"), &alias).unwrap();

		let rid = RepositoryId::at_common_dir(alias).unwrap();
		let listing = enumerate(&rid).await.unwrap();
		assert!(
			matches!(
				listing.entries[0].role,
				WorktreeRole::Primary { bare: false }
			),
			"{fmt}: an alias to an ordinary .git is not bare"
		);
		assert_eq!(
			listing.entries[0].path,
			canonical(&work),
			"{fmt}: the primary path is the real work tree, not the alias"
		);
		assert_eq!(listing.entries[0].head, Some(HeadKind::Symbolic));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn repository_identity_is_the_common_dir_anchor() {
	// The documented identity anchor is the shared common dir. The same repository discovered from its
	// primary vs a linked worktree yields different contextual `git_dir`/`worktree_root` but the same
	// `common_dir`, so the two identities must compare *equal*.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("identity-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git(&[
			"-C",
			work.to_str().unwrap(),
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);

		let from_main = RepositoryId::discover(&work).await.unwrap();
		let from_wt = RepositoryId::discover(&wt).await.unwrap();
		assert_ne!(
			from_main.git_dir(),
			from_wt.git_dir(),
			"{fmt}: the contextual git dirs differ"
		);
		assert_eq!(
			from_main, from_wt,
			"{fmt}: same repository (common-dir anchor) compares equal"
		);
		let explicit = RepositoryId::at_common_dir(canonical(&work.join(".git"))).unwrap();
		assert_eq!(
			from_main, explicit,
			"{fmt}: an explicit identity matches too"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symref_chain_through_a_one_level_pseudoref_resolves() {
	// git resolves `HEAD -> refs/heads/alias -> CUSTOM_REF -> refs/heads/feature` — a one-level pseudoref
	// (`CUSTOM_REF`) is a valid terminal/intermediate, even though `HEAD`'s *initial* target must be under
	// `refs/`. Enumeration must follow it, not reject the non-`refs/` hop.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("pseudoref-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		git(&["-C", w, "branch", "feature"]);
		git(&["-C", w, "symbolic-ref", "refs/heads/alias", "CUSTOM_REF"]);
		git(&["-C", w, "symbolic-ref", "CUSTOM_REF", "refs/heads/feature"]);
		git(&["-C", w, "symbolic-ref", "HEAD", "refs/heads/alias"]);
		// Oracle: git resolves the whole chain to feature.
		assert_eq!(
			git(&["-C", w, "symbolic-ref", "HEAD"]).trim(),
			"refs/heads/feature",
			"{fmt}: git resolves through the pseudoref"
		);

		let listing = enumerate(&rid_at(&work)).await.unwrap();
		assert_eq!(
			listing.entries[0].branch.as_deref(),
			Some("refs/heads/feature"),
			"{fmt}: enumeration follows the pseudoref to the terminal branch"
		);
		assert_eq!(listing.entries[0].head, Some(HeadKind::Symbolic));
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symlinked_admin_gitdir_file_is_followed() {
	// git follows a `gitdir` back-pointer that is itself a symlink to a regular pointer file — `worktree
	// list` and status still accept the worktree. Enumeration must not fail the whole repository over it.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("gitdir-symlink-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Replace the admin's `gitdir` file with a symlink to a relocated copy.
		let gitdir = work.join(".git/worktrees/wt/gitdir");
		std::fs::rename(&gitdir, work.join(".git/worktrees/wt/gitdir.real")).unwrap();
		std::os::unix::fs::symlink("gitdir.real", &gitdir).unwrap();
		// Oracle: git still accepts the worktree.
		assert!(
			git_ok(&["-C", wt.to_str().unwrap(), "status"]),
			"{fmt}: git follows the symlinked gitdir file"
		);

		let listing = enumerate(&rid_at(&work)).await.unwrap();
		assert!(
			listing
				.entries
				.iter()
				.any(|e| e.branch.as_deref() == Some("refs/heads/feature")),
			"{fmt}: the worktree with a symlinked gitdir file is still enumerated"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn identity_discovered_in_a_linked_worktree_survives_its_prune() {
	// A `RepositoryId` discovered from inside a linked worktree names that checkout's admin as its
	// `git_dir`. The shared repository stays valid if that worktree is later removed and pruned, so
	// enumeration (which reads *shared* state) must still work — it opens through the stable common dir,
	// never the now-gone per-worktree admin.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("prune-identity-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Discover the identity from *inside* the linked worktree (git_dir = the linked admin).
		let from_wt = RepositoryId::discover(&wt).await.unwrap();
		assert_ne!(from_wt.git_dir(), from_wt.common_dir());

		// Remove and prune the linked worktree — its admin (the identity's git_dir) is now gone.
		std::fs::remove_dir_all(&wt).unwrap();
		git(&["-C", w, "worktree", "prune"]);
		assert!(
			!from_wt.git_dir().exists(),
			"{fmt}: the pruned admin is gone"
		);

		// Enumeration via that identity still works — anchored on the surviving common dir.
		let listing = enumerate(&from_wt).await.unwrap();
		assert!(
			matches!(
				listing.entries[0].role,
				WorktreeRole::Primary { bare: false }
			),
			"{fmt}: the shared repository is still enumerable after the discovery worktree is pruned"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn stray_gitdir_and_locked_in_the_main_git_do_not_affect_the_primary() {
	// The primary git dir is not a linked admin: git ignores a stray `gitdir`/`locked` there. Enumeration
	// must derive the primary path directly (not from the stray `gitdir`) and keep it unlocked.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("main-stray-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		// Drop linked-admin-only files into the *main* `.git`.
		std::fs::write(work.join(".git/gitdir"), b"/bogus/checkout/.git\n").unwrap();
		std::fs::write(work.join(".git/locked"), b"not really locked\n").unwrap();

		let listing = enumerate(&rid_at(&work)).await.unwrap();
		let primary = &listing.entries[0];
		assert!(matches!(
			primary.role,
			WorktreeRole::Primary { bare: false }
		));
		assert_eq!(
			primary.path,
			canonical(&work),
			"{fmt}: the primary path is the work tree, not the stray gitdir target"
		);
		assert_eq!(
			primary.lock,
			LockState::Unlocked,
			"{fmt}: a stray `locked` in the main .git does not lock the primary"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_symlinked_worktrees_directory_is_not_followed() {
	// A symlinked `<common>/worktrees` would make every external child look like an ordinary admin, so
	// enumeration would dereference external `HEAD`/`locked`. It is fail-closed: a hard error, no leak.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("symlink-worktrees-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Relocate the whole `worktrees` dir and symlink to it; plant a secret lock in the (now external) admin.
		let worktrees = work.join(".git/worktrees");
		let external = base.join("external-worktrees");
		std::fs::rename(&worktrees, &external).unwrap();
		std::os::unix::fs::symlink(&external, &worktrees).unwrap();
		std::fs::write(external.join("wt/locked"), b"TOP SECRET").unwrap();

		assert!(
			enumerate(&rid_at(&work)).await.is_err(),
			"{fmt}: a symlinked worktrees dir must be a hard error, never followed"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn linked_worktrees_are_ordered_by_checkout_path() {
	// git's `worktree list` orders linked worktrees by *checkout path*, not admin name. Build two whose
	// admin-name order is the reverse of their path order and assert we match git.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("order-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		// Two axes: admin-name vs path order reversed (`wa` under zzz_dir, `wz` under aaa_dir), and a
		// case-differing pair (`B_wt` vs `a_wt`) whose byte order reverses git's ignorecase order on macOS.
		let a = base.join("zzz_dir/wa");
		let z = base.join("aaa_dir/wz");
		let bcase = base.join("B_wt");
		let acase = base.join("a_wt");
		std::fs::create_dir_all(a.parent().unwrap()).unwrap();
		std::fs::create_dir_all(z.parent().unwrap()).unwrap();
		git(&["-C", w, "worktree", "add", "-b", "ba", a.to_str().unwrap()]);
		git(&["-C", w, "worktree", "add", "-b", "bz", z.to_str().unwrap()]);
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"bb",
			bcase.to_str().unwrap(),
		]);
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"aa",
			acase.to_str().unwrap(),
		]);

		let listing = enumerate(&rid_at(&work)).await.unwrap();
		let linked: Vec<_> = listing
			.entries
			.iter()
			.filter(|e| matches!(e.role, WorktreeRole::Linked { .. }))
			.map(|e| e.path.clone())
			.collect();
		// git's order (by checkout path).
		let git_order: Vec<String> = git(&["-C", w, "worktree", "list", "--porcelain"])
			.lines()
			.filter_map(|l| l.strip_prefix("worktree "))
			.skip(1) // the primary
			.map(str::to_owned)
			.collect();
		let ours: Vec<String> = linked
			.iter()
			.map(|p| p.to_string_lossy().into_owned())
			.collect();
		let git_canon: Vec<String> = git_order
			.iter()
			.map(|p| {
				canonical(std::path::Path::new(p))
					.to_string_lossy()
					.into_owned()
			})
			.collect();
		assert_eq!(
			ours, git_canon,
			"{fmt}: linked worktrees follow git's checkout-path order"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn enumeration_does_not_dereference_a_symlinked_admin() {
	// Enumeration reads full per-worktree state, so it must NOT follow a *symlinked* admin (which points at
	// an external directory) — doing so would leak that directory's `HEAD`/`locked` into the listing. The
	// symlinked admin is excluded; enumeration still succeeds and exposes none of the external state.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("enum-symlink-admin-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Relocate the admin OUTSIDE worktrees/, symlink to it, and plant a secret lock reason.
		let admin = work.join(".git/worktrees/wt");
		let external = base.join("external-admin");
		std::fs::rename(&admin, &external).unwrap();
		std::os::unix::fs::symlink(&external, &admin).unwrap();
		std::fs::write(external.join("locked"), b"TOP SECRET").unwrap();

		let listing = enumerate(&rid_at(&work)).await.unwrap();
		assert!(
			listing.entries.iter().all(|e| e.lock
				!= LockState::Locked {
					reason: Some("TOP SECRET".to_owned())
				}),
			"{fmt}: the external admin's lock contents must not appear in the listing"
		);
		assert!(
			listing
				.entries
				.iter()
				.all(|e| !matches!(e.role, WorktreeRole::Linked { .. })),
			"{fmt}: the symlinked admin is not dereferenced/listed"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn a_stray_non_directory_in_the_worktrees_dir_is_ignored() {
	// git only treats *subdirectories* of `.git/worktrees/` as admin entries; a stray file (a `.DS_Store`,
	// a leftover lock) must be ignored, not fail enumeration/inspection for the whole repository.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("stray-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let w = work.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			w,
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);
		// Drop a stray non-directory entry alongside the real admin.
		std::fs::write(work.join(".git/worktrees/.DS_Store"), b"junk").unwrap();

		let listing = enumerate(&rid_at(&work)).await.unwrap();
		assert!(
			listing
				.entries
				.iter()
				.any(|e| matches!(e.role, WorktreeRole::Linked { .. })
					&& e.branch.as_deref() == Some("refs/heads/feature")),
			"{fmt}: the real linked worktree is still enumerated past the stray file"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn enumerates_a_bare_repository_hosting_linked_worktrees() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("enum-bare-{fmt}"));
		let src = base.join("src");
		init_repo(&src, fmt);
		commit_file(&src, "a.txt", "1\n", "init");
		let bare = base.join("bare.git");
		git(&[
			"clone",
			"--bare",
			"-q",
			src.to_str().unwrap(),
			bare.to_str().unwrap(),
		]);
		let b = bare.to_str().unwrap();
		let wt = base.join("wt");
		git(&[
			"-C",
			b,
			"worktree",
			"add",
			"-b",
			"wtbranch",
			wt.to_str().unwrap(),
			"HEAD",
		]);

		let listing = enumerate(&rid_bare(&bare)).await.unwrap();
		assert!(matches!(
			listing.entries[0].role,
			WorktreeRole::Primary { bare: true }
		));
		assert_eq!(
			listing.entries[0].head, None,
			"a bare primary has no checkout HEAD"
		);
		assert!(
			listing
				.entries
				.iter()
				.any(|e| matches!(e.role, WorktreeRole::Linked { .. })
					&& e.branch.as_deref() == Some("refs/heads/wtbranch")),
			"the linked worktree must be enumerated"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn bareness_without_core_bare_matches_git() {
	// An **unset** `core.bare` defaults to *non-bare* in git — the git-dir basename does not imply
	// bareness. Verified: even a bare clone with `core.bare` removed reports `--is-bare-repository=false`.
	// We must match git, not guess bare from the `.git`-suffixed name.
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("bare-infer-{fmt}"));
		let src = base.join("src");
		init_repo(&src, fmt);
		commit_file(&src, "a.txt", "1\n", "init");
		let bare = base.join("bare.git");
		git(&[
			"clone",
			"--bare",
			"-q",
			src.to_str().unwrap(),
			bare.to_str().unwrap(),
		]);
		// Remove the explicit `core.bare` key.
		let config = bare.join("config");
		let text: String = std::fs::read_to_string(&config)
			.unwrap()
			.lines()
			.filter(|l| !l.trim_start().starts_with("bare"))
			.map(|l| format!("{l}\n"))
			.collect();
		std::fs::write(&config, text).unwrap();

		// Oracle: git now reports the repository as non-bare.
		assert_eq!(
			git(&[
				"--git-dir",
				bare.to_str().unwrap(),
				"rev-parse",
				"--is-bare-repository"
			])
			.trim(),
			"false",
			"{fmt}: git treats an unset core.bare as non-bare"
		);
		let listing = enumerate(&rid_bare(&bare)).await.unwrap();
		assert!(
			matches!(
				listing.entries[0].role,
				WorktreeRole::Primary { bare: false }
			),
			"{fmt}: an unset core.bare is non-bare, matching git"
		);
		let _ = std::fs::remove_dir_all(&base);
	}
}

#[tokio::test]
async fn discovery_from_inside_a_linked_worktree_resolves_the_shared_common_dir() {
	for (fmt, _kind) in formats() {
		let base = unique_tmp(&format!("discover-{fmt}"));
		let work = base.join("repo");
		init_repo(&work, fmt);
		commit_file(&work, "a.txt", "1\n", "init");
		let wt = base.join("wt");
		git(&[
			"-C",
			work.to_str().unwrap(),
			"worktree",
			"add",
			"-b",
			"feature",
			wt.to_str().unwrap(),
		]);

		// Discovering from *inside* the linked worktree yields the shared `.git` as the common dir.
		let from_linked = RepositoryId::discover(&wt).await.unwrap();
		assert_eq!(from_linked.common_dir(), canonical(&work.join(".git")));
		// git_dir is this worktree's per-worktree admin dir, not the common dir.
		assert_ne!(from_linked.git_dir(), from_linked.common_dir());

		// Enumeration from that identity sees the same set as from the primary.
		let from_main = enumerate(&rid_at(&work)).await.unwrap();
		let from_wt = enumerate(&from_linked).await.unwrap();
		assert_eq!(from_main, from_wt);
		let _ = std::fs::remove_dir_all(&base);
	}
}
