#![cfg(unix)]
//! Attributes and line endings, checked against stock git: what `add` stores and warns of, what a
//! checkout writes, what status and diff see, the conversions gitana refuses, the attributes files
//! from outside the repository, and forced checkouts leaving clean files alone.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use gitana_file_store_local::{CapWorkDir, LocalFileStore};
use gitana_object::{ObjectId, Sha256};
use gitana_object_store::ObjectStore;
use gitana_path::GitPath;
use gitana_repository::Repository;
use gitana_test_git::git_command;
use gitana_worktree::{
	LineEndingChange, OutsideAttributes, ResetError, ResetMode, WorkTree, WorktreeError,
};

type Tree = WorkTree<LocalFileStore, CapWorkDir, Sha256>;
/// A case: its name, the config set, and `.gitattributes`.
type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
/// A refusal: its name, the config set, `.gitattributes`, and the attribute refused.
type Refusal<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str, &'a str);

/// The files every case stages or checks out: each kind of line ending git tells apart.
const FILES: &[(&str, &[u8])] = &[
	("lf.txt", b"one\ntwo\n"),
	("crlf.txt", b"one\r\ntwo\r\n"),
	("mixed.txt", b"one\r\ntwo\n"),
	("lonecr.txt", b"one\rtwo\n"),
	("noeol.txt", b"one\r\ntwo"),
	("nul.dat", b"one\0\r\ntwo\r\n"),
	("sub/deep.txt", b"deep\r\n"),
];

/// `add .` stores the same blobs as `git add -A`, and warns of the same files, for each way of
/// asking for line endings.
#[tokio::test]
async fn add_stores_and_warns_as_git_does() {
	let cases: &[Case] = &[
		("plain", &[], ""),
		("autocrlf-true", &[("core.autocrlf", "true")], ""),
		("autocrlf-input", &[("core.autocrlf", "input")], ""),
		("text", &[], "* text\n"),
		("text-auto", &[], "* text=auto\n"),
		("eol-crlf", &[], "*.txt eol=crlf\n"),
		("unset-text", &[("core.autocrlf", "true")], "*.txt -text\n"),
		(
			"binary-macro",
			&[("core.autocrlf", "true")],
			"*.txt binary\n",
		),
		("eol-lf", &[("core.autocrlf", "true")], "* text eol=lf\n"),
		("core-eol-crlf", &[("core.eol", "crlf")], "* text=auto\n"),
		("macro", &[], "[attr]norm text eol=lf\n*.txt norm\n"),
		("bom", &[], "\u{feff}*.txt text\n"),
		("folder", &[], "* -text\nsub/.gitattributes export-ignore\n"),
	];
	for (name, config, attributes) in cases {
		let Some((git_side, gitana_side)) = fresh_twins(&format!("add-{name}")) else {
			return;
		};
		for side in [&git_side, &gitana_side] {
			configure(side, config);
			write_files(side, FILES);
			if !attributes.is_empty() {
				std::fs::write(side.join(".gitattributes"), attributes).unwrap();
			}
			if *name == "folder" {
				std::fs::write(side.join("sub/.gitattributes"), "*.txt text\n").unwrap();
			}
		}
		let git_warnings = warnings(&git_ok(&git_side, &["add", "-A"]).stderr);
		let outcome = make_repo(&gitana_side)
			.add(&["."], "", false, None)
			.await
			.unwrap_or_else(|error| panic!("{name}: {error}"));
		assert_eq!(
			ls_files(&gitana_side),
			ls_files(&git_side),
			"{name}: the index"
		);
		let gitana_warnings: BTreeSet<(String, String)> = outcome
			.line_ending_warnings
			.iter()
			.map(|(path, change)| (path.to_string(), format!("{change:?}")))
			.collect();
		assert_eq!(gitana_warnings, git_warnings, "{name}: the warnings");
	}
}

/// A `.gitattributes` that `add` stages as deleted no longer governs the files it stages, as in
/// git; one left deleted but unstaged still does, from the index.
#[tokio::test]
async fn add_stops_reading_a_gitattributes_it_stages_as_deleted() {
	for (name, pathspecs, folder) in [
		("all", vec!["."], false),
		("only-the-file", vec!["a.txt"], false),
		("now-a-folder", vec!["."], true),
	] {
		let Some((git_side, gitana_side)) = fresh_twins(&format!("add-deleted-attrs-{name}")) else {
			return;
		};
		for side in [&git_side, &gitana_side] {
			std::fs::write(side.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
			std::fs::write(side.join("a.txt"), b"a\n").unwrap();
			git_ok(side, &["add", "-A"]);
			std::fs::remove_file(side.join(".gitattributes")).unwrap();
			if folder {
				std::fs::create_dir(side.join(".gitattributes")).unwrap();
				std::fs::write(side.join(".gitattributes/x"), b"x\n").unwrap();
			}
			std::fs::write(side.join("a.txt"), b"a\r\nb\r\n").unwrap();
		}
		let mut args = vec!["add"];
		args.extend(&pathspecs);
		git_ok(&git_side, &args);
		make_repo(&gitana_side)
			.add(&pathspecs, "", false, None)
			.await
			.unwrap();
		assert_eq!(ls_files(&gitana_side), ls_files(&git_side), "{name}");
	}
}

/// `check_materialise` refuses what `materialise_paths` couldn't write, before writing anything.
#[tokio::test]
async fn check_materialise_refuses_without_writing() {
	let Some((_, side)) = cloned_twins("check-materialise", "*.txt ident\n", &[]) else {
		return;
	};
	let wt = make_repo(&side);
	let tree = head_tree(&wt).await;
	let paths = [GitPath::from_utf8("lf.txt").unwrap()];
	let error = wt.check_materialise(tree, &paths).await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { attribute, .. } if attribute == "ident"),
		"{error:?}"
	);
	assert_eq!(worktree_files(&side), BTreeMap::new());
	wt.check_materialise(tree, &[GitPath::from_utf8("nul.dat").unwrap()])
		.await
		.unwrap();
	assert_eq!(worktree_files(&side), BTreeMap::new());
}

/// `add` leaves a tracked file whose stat still matches its entry as staged, as git does: no
/// repeated warning, and an unchanged file gitana can't convert doesn't make it refuse. A changed
/// one is refused.
#[tokio::test]
async fn add_leaves_unchanged_tracked_files_alone_as_git_does() {
	let Some((git_side, gitana_side)) = fresh_twins("add-unchanged") else {
		return;
	};
	for side in [&git_side, &gitana_side] {
		std::fs::write(side.join(".gitattributes"), "* text\n").unwrap();
		std::fs::write(side.join("crlf.txt"), b"one\r\ntwo\r\n").unwrap();
	}
	git_ok(&git_side, &["add", "-A"]);
	let wt = make_repo(&gitana_side);
	let first = wt.add(&["."], "", false, None).await.unwrap();
	assert_eq!(first.line_ending_warnings.len(), 1);
	// Past git's racy-timestamp window, where it re-stages a file written in the same second as
	// its index.
	std::thread::sleep(std::time::Duration::from_millis(1100));
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["update-index", "-q", "--refresh"]);
	}
	let again = git_ok(&git_side, &["add", "-A"]);
	assert_eq!(warnings(&again.stderr), BTreeSet::new(), "git, again");
	let again = wt.add(&["."], "", false, None).await.unwrap();
	assert_eq!(again.line_ending_warnings, [], "gitana, again");

	let config: &[(&str, &str)] = &[
		("filter.lfs.smudge", "tr o 0"),
		("filter.lfs.clean", "tr 0 o"),
	];
	let Some((_, side)) = cloned_twins("add-unchanged-lfs", "*.dat filter=lfs\n", config) else {
		return;
	};
	git_ok(&side, &["reset", "-q", "--hard"]);
	std::thread::sleep(std::time::Duration::from_millis(1100));
	git_ok(&side, &["update-index", "-q", "--refresh"]);
	let wt = make_repo(&side);
	wt.add(&["."], "", false, None).await.unwrap();
	assert_eq!(git_changes(&side), BTreeSet::new());
	std::fs::write(side.join("nul.dat"), b"changed\n").unwrap();
	let error = wt.add(&["."], "", false, None).await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { path, .. } if path.as_bytes() == b"nul.dat"),
		"{error:?}"
	);
}

/// `add`'s warnings come in git's order: files the index already had, then new ones, each by path.
#[tokio::test]
async fn add_warns_of_tracked_files_first_as_git_does() {
	let Some((git_side, gitana_side)) = fresh_twins("add-order") else {
		return;
	};
	for side in [&git_side, &gitana_side] {
		std::fs::write(side.join(".gitattributes"), "* text\n").unwrap();
		std::fs::write(side.join("z.txt"), b"z\n").unwrap();
		git_ok(side, &["add", "-A"]);
		std::fs::write(side.join("z.txt"), b"z\r\n").unwrap();
		std::fs::write(side.join("a.txt"), b"a\r\n").unwrap();
		std::fs::write(side.join("m.txt"), b"m\r\n").unwrap();
	}
	let stderr = git_ok(&git_side, &["add", "-A"]).stderr;
	let order: Vec<String> = String::from_utf8_lossy(&stderr)
		.lines()
		.map(|line| line.split('\'').nth(1).unwrap().to_owned())
		.collect();
	assert_eq!(order, ["z.txt", "a.txt", "m.txt"], "git's order");
	let outcome = make_repo(&gitana_side)
		.add(&["."], "", false, None)
		.await
		.unwrap();
	assert_eq!(
		outcome
			.line_ending_warnings
			.iter()
			.map(|(path, _)| path.to_string())
			.collect::<Vec<_>>(),
		order
	);
}

/// A checkout writes the same bytes as `git reset --hard`, and afterwards each side calls the
/// other's checkout what git calls it.
#[tokio::test]
async fn checkout_writes_and_status_reads_as_git_does() {
	let cases: &[Case] = &[
		("autocrlf-true", &[("core.autocrlf", "true")], ""),
		("autocrlf-input", &[("core.autocrlf", "input")], ""),
		("text-crlf", &[], "* text eol=crlf\n"),
		("auto-crlf", &[], "* text=auto eol=crlf\n"),
		("core-eol-crlf", &[("core.eol", "crlf")], "* text\n"),
		("unset-text", &[("core.autocrlf", "true")], "*.txt -text\n"),
		("binary", &[("core.autocrlf", "true")], "*.txt binary\n"),
		("plain", &[], ""),
	];
	for (name, config, attributes) in cases {
		let Some((git_side, gitana_side)) =
			cloned_twins(&format!("checkout-{name}"), attributes, config)
		else {
			return;
		};
		git_ok(&git_side, &["reset", "-q", "--hard"]);
		let wt = make_repo(&gitana_side);
		let tree = head_tree(&wt).await;
		wt.checkout(tree, true, None)
			.await
			.unwrap_or_else(|error| panic!("{name}: {error}"));
		assert_eq!(
			worktree_files(&gitana_side),
			worktree_files(&git_side),
			"{name}: the files written"
		);
		// Each side's view of the other's checkout.
		assert_eq!(
			gitana_changes(&make_repo(&git_side)).await,
			git_changes(&git_side),
			"{name}: gitana's status of git's checkout"
		);
		assert_eq!(
			git_changes(&gitana_side),
			gitana_changes(&wt).await,
			"{name}: git's status of gitana's checkout"
		);
	}
}

/// A file whose stat changed but whose content, as stored, didn't, is unchanged; an edited one is
/// changed, and diff shows it as it would be stored, as git's does.
#[tokio::test]
async fn status_and_diff_compare_as_stored_as_git_does() {
	let Some((_, side)) = cloned_twins("diff", "", &[("core.autocrlf", "true")]) else {
		return;
	};
	git_ok(&side, &["reset", "-q", "--hard"]);
	let wt = make_repo(&side);
	assert_eq!(
		std::fs::read(side.join("lf.txt")).unwrap(),
		b"one\r\ntwo\r\n"
	);
	// Rewritten with the same bytes, the stat no longer matches; the content as stored still does.
	std::thread::sleep(std::time::Duration::from_millis(20));
	std::fs::write(side.join("lf.txt"), b"one\r\ntwo\r\n").unwrap();
	assert_eq!(gitana_changes(&wt).await, BTreeSet::new());
	assert!(wt.diff_unstaged().await.unwrap().is_empty());

	std::fs::write(side.join("lf.txt"), b"one\r\ntwo\r\nthree\r\n").unwrap();
	assert_eq!(gitana_changes(&wt).await, git_changes(&side));
	let diffs = wt.diff_unstaged().await.unwrap();
	assert_eq!(diffs.len(), 1);
	let stored = git_ok(&side, &["hash-object", "-w", "--path=lf.txt", "lf.txt"]).stdout;
	let stored = String::from_utf8(stored).unwrap();
	let expected = git_ok(&side, &["cat-file", "blob", stored.trim()]).stdout;
	assert_eq!(diffs[0].new.as_ref().unwrap().0, expected);
}

/// `.gitattributes` asking for a conversion gitana won't do is refused, naming the attribute; a
/// filter with no driver is stored as it is, as git stores it.
#[tokio::test]
async fn refuses_the_conversions_it_cannot_do() {
	let refused: &[Refusal] = &[
		(
			"driver",
			&[("filter.lfs.clean", "cat"), ("filter.lfs.smudge", "cat")],
			"*.txt filter=lfs\n",
			"filter=lfs",
		),
		(
			"process",
			&[("filter.lfs.process", "git-lfs filter-process")],
			"*.txt filter=lfs\n",
			"filter=lfs",
		),
		(
			"required",
			&[("filter.req.required", "true")],
			"*.txt filter=req\n",
			"filter=req",
		),
		("ident", &[], "*.txt ident\n", "ident"),
		(
			"encoding",
			&[],
			"*.txt working-tree-encoding=UTF-16\n",
			"working-tree-encoding=UTF-16",
		),
	];
	for (name, config, attributes, attribute) in refused {
		let Some((_, side)) = fresh_twins(&format!("refuse-{name}")) else {
			return;
		};
		configure(&side, config);
		std::fs::write(side.join("a.txt"), b"$Id$\n").unwrap();
		std::fs::write(side.join(".gitattributes"), attributes).unwrap();
		let error = make_repo(&side)
			.add(&["a.txt"], "", false, None)
			.await
			.expect_err(name);
		match error {
			WorktreeError::UnsupportedAttribute {
				path,
				attribute: found,
			} => {
				assert_eq!(path.to_string(), "a.txt", "{name}");
				assert_eq!(found, *attribute, "{name}");
			}
			other => panic!("{name}: {other:?}"),
		}
		assert_eq!(ls_files(&side), "", "{name}: nothing staged");
	}

	// No driver configured, not required: stored as it is, as git stores it; a UTF-8
	// working-tree-encoding converts nothing.
	let Some((git_side, gitana_side)) = fresh_twins("refuse-none") else {
		return;
	};
	for side in [&git_side, &gitana_side] {
		write_files(side, FILES);
		std::fs::write(
			side.join(".gitattributes"),
			"*.txt filter=nodriver text\n*.dat working-tree-encoding=utf-8\n",
		)
		.unwrap();
	}
	git_ok(&git_side, &["add", "-A"]);
	make_repo(&gitana_side)
		.add(&["."], "", false, None)
		.await
		.unwrap();
	assert_eq!(ls_files(&gitana_side), ls_files(&git_side));
}

/// A checkout that would write a file through a driver is refused before it writes anything.
#[tokio::test]
async fn a_checkout_it_cannot_convert_writes_nothing() {
	let Some((_, side)) = cloned_twins(
		"refuse-checkout",
		"*.txt filter=lfs\n",
		&[("filter.lfs.smudge", "cat"), ("filter.lfs.clean", "cat")],
	) else {
		return;
	};
	let wt = make_repo(&side);
	let tree = head_tree(&wt).await;
	let error = wt.checkout(tree, true, None).await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { attribute, .. } if attribute == "filter=lfs"),
		"{error:?}"
	);
	assert_eq!(worktree_files(&side), BTreeMap::new());
}

/// The system and global attributes files rank below the repository's own, as git's do: the
/// global file through `core.attributesFile` for git, handed in for gitana.
#[tokio::test]
async fn outside_attribute_files_rank_as_git_does() {
	let Some((git_side, gitana_side)) = fresh_twins("outside") else {
		return;
	};
	let global = git_side.parent().unwrap().join("global-attributes");
	let global_text = "*.txt text\nlonecr.txt -text\n";
	std::fs::write(&global, global_text).unwrap();
	for side in [&git_side, &gitana_side] {
		write_files(side, FILES);
		std::fs::write(side.join(".gitattributes"), "mixed.txt -text\n").unwrap();
		std::fs::create_dir_all(side.join(".git/info")).unwrap();
		std::fs::write(side.join(".git/info/attributes"), "crlf.txt -text\n").unwrap();
	}
	configure(
		&git_side,
		&[("core.attributesFile", global.to_str().unwrap())],
	);
	git_ok(&git_side, &["add", "-A"]);
	make_repo(&gitana_side)
		.with_outside_attributes(OutsideAttributes {
			// Lowest of all: everything it says, the global file overrides.
			system: Some(b"*.txt -text\nnoeol.txt text\n".to_vec()),
			global: Some(global_text.as_bytes().to_vec()),
		})
		.add(&["."], "", false, None)
		.await
		.unwrap();
	assert_eq!(ls_files(&gitana_side), ls_files(&git_side));

	// The system file applies where nothing above it speaks.
	let Some((_, side)) = fresh_twins("outside-system") else {
		return;
	};
	write_files(&side, FILES);
	make_repo(&side)
		.with_outside_attributes(OutsideAttributes {
			system: Some(b"crlf.txt text\n".to_vec()),
			global: None,
		})
		.add(&["crlf.txt"], "", false, None)
		.await
		.unwrap();
	assert_eq!(
		git_ok(&side, &["cat-file", "blob", ":crlf.txt"]).stdout,
		b"one\ntwo\n"
	);
}

/// Switching reads the target's `.gitattributes` before the working tree's, as git does.
#[tokio::test]
async fn a_switch_writes_by_the_target_s_attributes_as_git_does() {
	let Some((git_side, gitana_side)) = cloned_twins("switch-attrs", "*.txt -text\n", &[]) else {
		return;
	};
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["reset", "-q", "--hard"]);
	}
	// A branch whose `.gitattributes` asks for CRLF, adding a file.
	let origin = git_side.parent().unwrap().join("origin");
	git_ok(&origin, &["switch", "-q", "-c", "crlf"]);
	std::fs::write(origin.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
	std::fs::write(origin.join("new.txt"), "new\n").unwrap();
	commit_all(&origin, "crlf");
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["fetch", "-q", "origin", "crlf:crlf"]);
	}
	git_ok(&git_side, &["switch", "-q", "crlf"]);
	let wt = make_repo(&gitana_side);
	let head = head_tree(&wt).await;
	let target = commit_tree(&wt, "crlf").await;
	wt.checkout_merge(head, target, None).await.unwrap();
	assert_eq!(
		std::fs::read(gitana_side.join("new.txt")).unwrap(),
		std::fs::read(git_side.join("new.txt")).unwrap()
	);
	assert_eq!(
		std::fs::read(gitana_side.join("new.txt")).unwrap(),
		b"new\r\n"
	);
	// A file the switch doesn't change isn't rewritten, by either.
	assert_eq!(
		std::fs::read(gitana_side.join("lf.txt")).unwrap(),
		std::fs::read(git_side.join("lf.txt")).unwrap()
	);
}

/// A switch's check for local changes compares files as they'd be stored, as git's does: a
/// converted file whose stat changed but whose content didn't isn't in the way.
#[tokio::test]
async fn a_switch_sees_a_converted_file_as_clean_as_git_does() {
	let Some((git_side, gitana_side)) =
		cloned_twins("switch-clean", "", &[("core.autocrlf", "true")])
	else {
		return;
	};
	let origin = git_side.parent().unwrap().join("origin");
	git_ok(&origin, &["switch", "-q", "-c", "edit"]);
	std::fs::write(origin.join("lf.txt"), "one\ntwo\nthree\n").unwrap();
	commit_all(&origin, "edit");
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["reset", "-q", "--hard"]);
		git_ok(side, &["fetch", "-q", "origin", "edit:edit"]);
		// Rewritten with the same bytes: the stat changes, the content as stored doesn't.
		std::thread::sleep(std::time::Duration::from_millis(20));
		std::fs::write(side.join("lf.txt"), b"one\r\ntwo\r\n").unwrap();
	}
	git_ok(&git_side, &["switch", "-q", "edit"]);
	let wt = make_repo(&gitana_side);
	let head = head_tree(&wt).await;
	let target = commit_tree(&wt, "edit").await;
	wt.checkout_merge(head, target, None).await.unwrap();
	assert_eq!(
		std::fs::read(gitana_side.join("lf.txt")).unwrap(),
		std::fs::read(git_side.join("lf.txt")).unwrap()
	);

	// A plain (not forced) checkout's guard compares the same way.
	git_ok(&gitana_side, &["reset", "-q", "--hard"]);
	std::thread::sleep(std::time::Duration::from_millis(20));
	std::fs::write(gitana_side.join("lf.txt"), b"one\r\ntwo\r\n").unwrap();
	wt.checkout(target, false, None).await.unwrap();
	assert_eq!(
		std::fs::read(gitana_side.join("lf.txt")).unwrap(),
		std::fs::read(git_side.join("lf.txt")).unwrap()
	);
}

/// A filter is refused only the way its driver runs, as git runs only that one: a clean-only
/// driver refuses staging but not checkout, a smudge-only one the reverse.
#[tokio::test]
async fn a_filter_is_refused_only_the_way_its_driver_runs() {
	let config: &[(&str, &str)] = &[("filter.in.clean", "cat"), ("filter.out.smudge", "cat")];
	let Some((_, side)) = cloned_twins(
		"filter-direction",
		"lf.txt filter=in\ncrlf.txt filter=out\n",
		config,
	) else {
		return;
	};
	let wt = make_repo(&side);
	let tree = head_tree(&wt).await;
	let error = wt.checkout(tree, true, None).await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { attribute, .. } if attribute == "filter=out"),
		"{error:?}"
	);
	git_ok(&side, &["reset", "-q", "--hard"]);
	std::fs::write(side.join("crlf.txt"), b"changed\n").unwrap();
	std::fs::write(side.join("lf.txt"), b"changed\n").unwrap();
	let error = wt.add(&["lf.txt"], "", false, None).await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { attribute, .. } if attribute == "filter=in"),
		"{error:?}"
	);
	wt.add(&["crlf.txt"], "", false, None).await.unwrap();
}

/// A switch, and a population, that would write a file through a driver are refused before they
/// write anything.
#[tokio::test]
async fn a_switch_or_population_it_cannot_convert_writes_nothing() {
	let config: &[(&str, &str)] = &[("filter.lfs.smudge", "cat"), ("filter.lfs.clean", "cat")];
	let Some((side, fresh)) = cloned_twins("refuse-switch", "", config) else {
		return;
	};
	let origin = side.parent().unwrap().join("origin");
	git_ok(&origin, &["switch", "-q", "-c", "lfs"]);
	std::fs::write(origin.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
	std::fs::write(origin.join("x.bin"), "x\n").unwrap();
	std::fs::write(origin.join("lf.txt"), "changed\n").unwrap();
	commit_all(&origin, "lfs");
	git_ok(&side, &["reset", "-q", "--hard"]);
	git_ok(&side, &["fetch", "-q", "origin", "lfs:lfs"]);
	let wt = make_repo(&side);
	let head = head_tree(&wt).await;
	let target = commit_tree(&wt, "lfs").await;
	let before = worktree_files(&side);
	let error = wt.checkout_merge(head, target, None).await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { path, .. } if path.as_bytes() == b"x.bin"),
		"{error:?}"
	);
	assert_eq!(
		worktree_files(&side),
		before,
		"nothing written by the switch"
	);

	git_ok(&fresh, &["fetch", "-q", "origin", "lfs:lfs"]);
	let wt = make_repo(&fresh);
	let target = commit_tree(&wt, "lfs").await;
	let error = wt.populate(target).await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { path, .. } if path.as_bytes() == b"x.bin"),
		"{error:?}"
	);
	assert_eq!(
		worktree_files(&fresh),
		BTreeMap::new(),
		"nothing written by populate"
	);
}

/// Moving to a tree without the `.gitattributes` the working tree has writes as git does. A switch
/// and a checkout remove before they write, so the file they remove governs nothing: a removed
/// `eol=crlf` writes LF, and a removed `ident` doesn't block them. A restore writes first, so its
/// rules still apply: CRLF, and `ident` refused. Each against git's own command.
#[tokio::test]
async fn moving_away_from_attributes_writes_as_git_does() {
	for (name, attributes) in [
		("crlf", "*.txt text eol=crlf\n"),
		("ident", "*.txt ident\n"),
	] {
		for operation in ["switch", "checkout", "restore"] {
			let what = format!("{name} {operation}");
			let Some((git_side, gitana_side)) =
				cloned_twins(&format!("away-{name}-{operation}"), attributes, &[])
			else {
				return;
			};
			let origin = git_side.parent().unwrap().join("origin");
			git_ok(&origin, &["switch", "-q", "-c", "plain", "HEAD~1"]);
			std::fs::write(origin.join("lf.txt"), "changed\n").unwrap();
			commit_all(&origin, "plain");
			for side in [&git_side, &gitana_side] {
				git_ok(side, &["reset", "-q", "--hard"]);
				git_ok(side, &["fetch", "-q", "origin", "plain:plain"]);
			}
			let wt = make_repo(&gitana_side);
			let head = head_tree(&wt).await;
			let target = commit_tree(&wt, "plain").await;
			let done = match operation {
				"switch" => {
					git_ok(&git_side, &["switch", "-q", "plain"]);
					wt.checkout_merge(head, target, None).await
				}
				"checkout" => {
					git_ok(&git_side, &["read-tree", "--reset", "-u", "plain"]);
					wt.checkout(target, true, None).await
				}
				_ => {
					git_ok(
						&git_side,
						&["restore", "--source=plain", "--staged", "--worktree", "."],
					);
					wt.restore(Some(target), true, true, &["."], "").await
				}
			};
			if operation == "restore" && name == "ident" {
				assert!(
					matches!(&done, Err(WorktreeError::UnsupportedAttribute { attribute, .. }) if attribute == "ident"),
					"{what}: {done:?}"
				);
				continue;
			}
			done.unwrap_or_else(|error| panic!("{what}: {error}"));
			assert_eq!(
				std::fs::read(gitana_side.join("lf.txt")).unwrap(),
				std::fs::read(git_side.join("lf.txt")).unwrap(),
				"{what}"
			);
			assert_eq!(
				gitana_side.join(".gitattributes").exists(),
				git_side.join(".gitattributes").exists(),
				"{what}"
			);
		}
	}
}

/// `sparse-checkout set` that would bring back a file gitana can't convert refuses before it changes
/// anything: the old patterns, the files, the index, and no lock left.
#[tokio::test]
async fn a_sparse_set_it_cannot_apply_changes_nothing() {
	let Some((_, side)) = cloned_twins("sparse-set", "sub/* ident\n", &[]) else {
		return;
	};
	git_ok(&side, &["reset", "-q", "--hard"]);
	std::fs::create_dir_all(side.join("keep")).unwrap();
	git_ok(&side, &["sparse-checkout", "set", "--cone", "keep"]);
	assert!(!side.join("sub/deep.txt").exists());
	let patterns = std::fs::read(side.join(".git/info/sparse-checkout")).unwrap();
	let index = ls_files(&side);
	let wt = make_repo(&side);
	let error = wt
		.apply_sparse_set(&gitana_worktree::SparseSet::cone_utf8(["keep", "sub"]).unwrap())
		.await
		.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { attribute, .. } if attribute == "ident"),
		"{error:?}"
	);
	assert_eq!(
		std::fs::read(side.join(".git/info/sparse-checkout")).unwrap(),
		patterns
	);
	assert_eq!(ls_files(&side), index);
	assert!(!side.join("sub/deep.txt").exists());
	assert!(!side.join(".git/index.lock").exists());
}

/// A plain checkout refuses to overwrite a file whose size changed, even when its content as stored
/// didn't, as git's one-tree `read-tree -m -u` does (git's size shortcut).
#[tokio::test]
async fn a_checkout_refuses_over_a_size_change_as_git_does() {
	let Some((git_side, gitana_side)) = cloned_twins("size-checkout", "lf.txt text eol=crlf\n", &[])
	else {
		return;
	};
	let origin = git_side.parent().unwrap().join("origin");
	git_ok(&origin, &["switch", "-q", "-c", "edit"]);
	std::fs::write(origin.join("lf.txt"), "edited\n").unwrap();
	commit_all(&origin, "edit");
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["reset", "-q", "--hard"]);
		git_ok(side, &["fetch", "-q", "origin", "edit:edit"]);
	}
	// A second on, the CRLF file is rewritten as LF: a new size, the same content as stored.
	std::thread::sleep(std::time::Duration::from_millis(1100));
	for side in [&git_side, &gitana_side] {
		std::fs::write(side.join("lf.txt"), b"one\ntwo\n").unwrap();
	}
	let git = git_out(&git_side, &["read-tree", "-m", "-u", "edit"]);
	assert!(!git.status.success(), "git refuses");
	let wt = make_repo(&gitana_side);
	let target = commit_tree(&wt, "edit").await;
	let error = wt.checkout(target, false, None).await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::Conflict(path) if path.as_bytes() == b"lf.txt"),
		"{error:?}"
	);
	assert_eq!(
		std::fs::read(gitana_side.join("lf.txt")).unwrap(),
		b"one\ntwo\n"
	);
}

/// `core.safecrlf=true` refuses a file whose line endings wouldn't round-trip, with git's message.
#[tokio::test]
async fn safecrlf_true_refuses_as_git_does() {
	let Some((git_side, gitana_side)) = fresh_twins("safecrlf") else {
		return;
	};
	for side in [&git_side, &gitana_side] {
		configure(side, &[("core.safecrlf", "true")]);
		std::fs::write(side.join("mixed.txt"), b"one\r\ntwo\n").unwrap();
		std::fs::write(side.join(".gitattributes"), "* text\n").unwrap();
	}
	let refused = git_out(&git_side, &["add", "mixed.txt"]);
	assert!(!refused.status.success());
	let error = make_repo(&gitana_side)
		.add(&["mixed.txt"], "", false, None)
		.await
		.unwrap_err();
	assert!(matches!(
		error,
		WorktreeError::IrreversibleLineEndings {
			change: LineEndingChange::CrlfToLf,
			..
		}
	));
	assert_eq!(
		String::from_utf8_lossy(&refused.stderr).trim(),
		format!("fatal: {error}")
	);
	assert_eq!(ls_files(&gitana_side), "");
}

/// A forced checkout leaves alone a file the index already holds at the target's version while its
/// stat still matches, as git does: it isn't rewritten, and a clean file gitana can't convert isn't
/// refused. A changed one is, before anything is written.
#[tokio::test]
async fn forced_checkouts_leave_clean_files_alone_as_git_does() {
	// A filter that changes the bytes: `o` is `0` in the working tree.
	let config: &[(&str, &str)] = &[
		("filter.lfs.smudge", "tr o 0"),
		("filter.lfs.clean", "tr 0 o"),
	];
	let Some((git_side, gitana_side)) = cloned_twins("clean", "*.dat filter=lfs\n", config) else {
		return;
	};
	// git checks both out (gitana won't write the filtered file), then, a second on, refreshes the
	// index so no entry is racily clean.
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["reset", "-q", "--hard"]);
	}
	std::thread::sleep(std::time::Duration::from_millis(1100));
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["update-index", "-q", "--refresh"]);
	}
	let git_before = stamps(&git_side);
	let before = stamps(&gitana_side);
	std::thread::sleep(std::time::Duration::from_millis(20));
	git_ok(&git_side, &["reset", "-q", "--hard"]);
	assert_eq!(stamps(&git_side), git_before, "git rewrites nothing");
	let wt = make_repo(&gitana_side);
	wt.reset("HEAD", ResetMode::Hard, &TestIdentity, None)
		.await
		.unwrap();
	assert_eq!(stamps(&gitana_side), before, "nothing rewritten by gitana");
	wt.restore(None, true, false, &["."], "").await.unwrap();
	assert_eq!(stamps(&gitana_side), before, "nothing rewritten by restore");

	// The hash checks can't tell the filtered file is clean, so count it changed; status can, by its
	// stat.
	assert_eq!(
		wt.diverged_tracked_content_paths()
			.await
			.unwrap()
			.iter()
			.map(ToString::to_string)
			.collect::<Vec<_>>(),
		["nul.dat"]
	);
	assert_eq!(gitana_changes(&wt).await, BTreeSet::new());

	// Changed, it would need writing: refused, and nothing else is written either.
	std::fs::write(gitana_side.join("nul.dat"), b"changed\n").unwrap();
	std::fs::write(gitana_side.join("lf.txt"), b"changed\n").unwrap();
	let error = wt
		.reset("HEAD", ResetMode::Hard, &TestIdentity, None)
		.await
		.unwrap_err();
	assert!(
		matches!(&error, ResetError::Worktree(WorktreeError::UnsupportedAttribute { path, .. }) if path.as_bytes() == b"nul.dat"),
		"{error:?}"
	);
	assert_eq!(
		std::fs::read(gitana_side.join("lf.txt")).unwrap(),
		b"changed\n"
	);
	let error = wt.restore(None, true, false, &["."], "").await.unwrap_err();
	assert!(
		matches!(&error, WorktreeError::UnsupportedAttribute { path, .. } if path.as_bytes() == b"nul.dat"),
		"{error:?}"
	);
	assert_eq!(
		std::fs::read(gitana_side.join("lf.txt")).unwrap(),
		b"changed\n"
	);
}

/// A forced checkout that leaves a clean file alone still clears a skip-worktree bit set by hand on a
/// path the sparse patterns include, as git does; the file can then be staged.
#[tokio::test]
async fn a_forced_checkout_clears_a_stray_skip_worktree_bit_as_git_does() {
	let Some((git_side, gitana_side)) = cloned_twins("stray-skip", "", &[]) else {
		return;
	};
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["reset", "-q", "--hard"]);
		git_ok(side, &["sparse-checkout", "set", "--cone", "sub"]);
	}
	// A second on, so the entries aren't racy and the clean-file skip applies.
	std::thread::sleep(std::time::Duration::from_millis(1100));
	for side in [&git_side, &gitana_side] {
		git_ok(side, &["update-index", "--skip-worktree", "lf.txt"]);
	}
	git_ok(&git_side, &["reset", "-q", "--hard"]);
	let wt = make_repo(&gitana_side);
	wt.reset("HEAD", ResetMode::Hard, &TestIdentity, None)
		.await
		.unwrap();
	let tags = |side: &Path| String::from_utf8(git_ok(side, &["ls-files", "-t"]).stdout).unwrap();
	assert_eq!(tags(&gitana_side), tags(&git_side));
	std::fs::write(gitana_side.join("lf.txt"), b"edited\n").unwrap();
	wt.add(&["lf.txt"], "", false, None).await.unwrap();
	assert_eq!(
		git_ok(&gitana_side, &["cat-file", "blob", ":lf.txt"]).stdout,
		b"edited\n"
	);
}

/// A moved file keeps its stat cache only if its content, as stored by the attributes that apply
/// once the move is done, still matches.
#[tokio::test]
async fn mv_judges_a_moved_file_as_stored() {
	let Some((_, side)) = cloned_twins("mv", "", &[("core.autocrlf", "true")]) else {
		return;
	};
	git_ok(&side, &["reset", "-q", "--hard"]);
	let wt = make_repo(&side);
	wt.mv(&["lf.txt"], "moved.txt", "", false, false)
		.await
		.unwrap();
	let index = wt.load_index().await.unwrap();
	let entry = index
		.entry(&GitPath::from_utf8("moved.txt").unwrap())
		.unwrap();
	assert_ne!(
		entry.stat,
		gitana_worktree::Stat::default(),
		"a clean move keeps a real stat"
	);
	assert_eq!(gitana_changes(&wt).await, git_changes(&side));
}

/// Populating a fresh checkout writes each file converted, as git's checkout does.
#[tokio::test]
async fn populate_writes_as_git_does() {
	let Some((git_side, gitana_side)) = cloned_twins("populate", "", &[("core.autocrlf", "true")])
	else {
		return;
	};
	git_ok(&git_side, &["reset", "-q", "--hard"]);
	let wt = make_repo(&gitana_side);
	let tree = head_tree(&wt).await;
	wt.populate(tree).await.unwrap();
	assert_eq!(worktree_files(&gitana_side), worktree_files(&git_side));
	assert_eq!(git_changes(&gitana_side), BTreeSet::new());
}

// --- helpers -------------------------------------------------------------------------------------

/// An identity with fixed lines.
struct TestIdentity;

impl gitana_identity::Identity for TestIdentity {
	async fn author(&self) -> anyhow::Result<String> {
		Ok("A U Thor <author@example.com> 1700000000 +0000".to_owned())
	}
	async fn committer(&self) -> anyhow::Result<String> {
		Ok("C O Mitter <committer@example.com> 1700000000 +0000".to_owned())
	}
	async fn committer_or_default(&self) -> anyhow::Result<String> {
		self.committer().await
	}
}

fn open_dir(path: impl AsRef<Path>) -> cap_std::fs::Dir {
	cap_std::fs::Dir::open_ambient_dir(path.as_ref(), cap_std::ambient_authority()).unwrap()
}

fn make_repo(work: &Path) -> Tree {
	let git_dir = work.join(".git");
	let repo = Repository::new(ObjectStore::<_, Sha256>::new(LocalFileStore::from_dir(
		open_dir(&git_dir),
	)));
	WorkTree::new(repo, CapWorkDir::from_dir(open_dir(work)), git_dir)
}

/// Two empty repositories, one for git and one for gitana; `None` where git can't make sha256
/// repositories.
fn fresh_twins(tag: &str) -> Option<(PathBuf, PathBuf)> {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return None;
	}
	let base = unique_tmp(tag);
	let sides = (base.join("git"), base.join("gitana"));
	for side in [&sides.0, &sides.1] {
		git(&[
			"init",
			"--object-format=sha256",
			"-q",
			"-b",
			"main",
			side.to_str().unwrap(),
		]);
		configure(side, &[("core.autocrlf", "false")]);
	}
	Some(sides)
}

/// An origin whose first commit holds [`FILES`] as they are, and whose second adds `attributes` as
/// `.gitattributes` (so the stored blobs keep their line endings); and two clones of it with no
/// checkout, configured by `config`.
fn cloned_twins(
	tag: &str,
	attributes: &str,
	config: &[(&str, &str)],
) -> Option<(PathBuf, PathBuf)> {
	if !git_supports_sha256() {
		eprintln!("skipping: git without --object-format=sha256");
		return None;
	}
	let base = unique_tmp(tag);
	let origin = base.join("origin");
	git(&[
		"init",
		"--object-format=sha256",
		"-q",
		"-b",
		"main",
		origin.to_str().unwrap(),
	]);
	configure(&origin, &[("core.autocrlf", "false")]);
	write_files(&origin, FILES);
	commit_all(&origin, "files");
	if !attributes.is_empty() {
		std::fs::write(origin.join(".gitattributes"), attributes).unwrap();
		commit_all(&origin, "attributes");
	}
	let sides = (base.join("git"), base.join("gitana"));
	for side in [&sides.0, &sides.1] {
		git(&[
			"clone",
			"-q",
			"--no-checkout",
			origin.to_str().unwrap(),
			side.to_str().unwrap(),
		]);
		configure(side, &[("core.autocrlf", "false")]);
		configure(side, config);
	}
	Some(sides)
}

fn configure(side: &Path, config: &[(&str, &str)]) {
	for (key, value) in config {
		git_ok(side, &["config", key, value]);
	}
}

fn write_files(side: &Path, files: &[(&str, &[u8])]) {
	for (path, content) in files {
		let path = side.join(path);
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, content).unwrap();
	}
}

async fn head_tree(wt: &Tree) -> ObjectId<Sha256> {
	let head = wt
		.repository()
		.refs()
		.resolve_head()
		.await
		.unwrap()
		.unwrap();
	wt.repository().commit_tree(head).await.unwrap()
}

async fn commit_tree(wt: &Tree, branch: &str) -> ObjectId<Sha256> {
	let commit = wt
		.repository()
		.refs()
		.resolve(&format!("refs/heads/{branch}"))
		.await
		.unwrap()
		.unwrap();
	wt.repository().commit_tree(commit).await.unwrap()
}

/// `git ls-files -s`: each staged path's mode and blob.
fn ls_files(side: &Path) -> String {
	String::from_utf8(git_ok(side, &["ls-files", "-s"]).stdout).unwrap()
}

/// Every file in the working tree, by path, outside `.git`.
fn worktree_files(side: &Path) -> BTreeMap<String, Vec<u8>> {
	fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
		for entry in std::fs::read_dir(dir).unwrap() {
			let entry = entry.unwrap();
			let path = entry.path();
			if path.file_name().unwrap() == ".git" {
				continue;
			}
			if entry.file_type().unwrap().is_dir() {
				walk(root, &path, out);
			} else {
				let rel = path
					.strip_prefix(root)
					.unwrap()
					.to_str()
					.unwrap()
					.to_owned();
				out.insert(rel, std::fs::read(&path).unwrap());
			}
		}
	}
	let mut out = BTreeMap::new();
	walk(side, side, &mut out);
	out
}

/// Each working file's inode and modification time, to see what was rewritten.
fn stamps(side: &Path) -> BTreeMap<String, (u64, i64, i64)> {
	worktree_files(side)
		.keys()
		.map(|path| {
			let meta = std::fs::metadata(side.join(path)).unwrap();
			(path.clone(), (meta.ino(), meta.mtime(), meta.mtime_nsec()))
		})
		.collect()
}

/// gitana's worktree column: each tracked path whose working file differs from the index.
async fn gitana_changes(wt: &Tree) -> BTreeSet<(String, char)> {
	wt.status(None)
		.await
		.unwrap()
		.changed
		.into_iter()
		.filter(|entry| entry.worktree != ' ')
		.map(|entry| (entry.path.to_string(), entry.worktree))
		.collect()
}

/// git's worktree column, from `git status --porcelain`.
fn git_changes(side: &Path) -> BTreeSet<(String, char)> {
	String::from_utf8(git_ok(side, &["status", "--porcelain", "--untracked-files=no"]).stdout)
		.unwrap()
		.lines()
		.filter_map(|line| {
			let worktree = line.chars().nth(1).unwrap();
			(worktree != ' ').then(|| (line[3..].to_owned(), worktree))
		})
		.collect()
}

/// git's `core.safecrlf=warn` warnings in `stderr`, each change by its [`LineEndingChange`] name.
fn warnings(stderr: &[u8]) -> BTreeSet<(String, String)> {
	String::from_utf8_lossy(stderr)
		.lines()
		.filter_map(|line| {
			let rest = line.strip_prefix("warning: in the working copy of '")?;
			let (path, rest) = rest.split_once("', ")?;
			let change = if rest.starts_with("CRLF will be replaced by LF") {
				LineEndingChange::CrlfToLf
			} else if rest.starts_with("LF will be replaced by CRLF") {
				LineEndingChange::LfToCrlf
			} else {
				panic!("an unknown warning: {line}")
			};
			Some((path.to_owned(), format!("{change:?}")))
		})
		.collect()
}

fn commit_all(side: &Path, message: &str) {
	git_ok(side, &["add", "-A"]);
	git_ok(
		side,
		&[
			"-c",
			"user.name=T",
			"-c",
			"user.email=t@e",
			"-c",
			"core.hooksPath=/dev/null",
			"commit",
			"-q",
			"-m",
			message,
		],
	);
}

fn git_out(side: &Path, args: &[&str]) -> Output {
	git_command()
		.arg("-C")
		.arg(side)
		.args(args)
		.output()
		.expect("run git")
}

fn git_ok(side: &Path, args: &[&str]) -> Output {
	let out = git_out(side, args);
	assert!(
		out.status.success(),
		"git {args:?} failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	out
}

fn git(args: &[&str]) -> String {
	let out = git_command().args(args).output().expect("run git");
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
	let dir = std::env::temp_dir().join(format!(
		"gitana-line-endings-{tag}-{}-{seq}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

fn git_supports_sha256() -> bool {
	use std::sync::OnceLock;
	static SUPPORTED: OnceLock<bool> = OnceLock::new();
	*SUPPORTED.get_or_init(|| {
		let probe = unique_tmp("probe");
		let ok = git_command()
			.args(["init", "--object-format=sha256", probe.to_str().unwrap()])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false);
		let _ = std::fs::remove_dir_all(&probe);
		ok
	})
}
