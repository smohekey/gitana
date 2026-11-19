// Exercises the native cap-std backend; the wasm target has no `from_dir`.
#![cfg(not(target_arch = "wasm32"))]

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use gitana_file_store_local::{CapWorkDir, PopulationEntry, WorkDirFs, WorkEntryIdentity};
use gitana_path::GitPath;

fn work_dir(tag: &str) -> (std::path::PathBuf, CapWorkDir) {
	let dir = std::env::temp_dir().join(format!(
		"gitana-work-entry-identity-{tag}-{}",
		std::process::id()
	));
	let _ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(dir.join("a/b")).unwrap();
	let work = CapWorkDir::from_dir(Dir::open_ambient_dir(&dir, ambient_authority()).unwrap());
	(dir, work)
}

fn path(text: &str) -> GitPath {
	GitPath::from_utf8(text).unwrap()
}

fn names(dir: &std::path::Path) -> Vec<String> {
	let mut names: Vec<String> = std::fs::read_dir(dir)
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.collect();
	names.sort();
	names
}

/// A file recorded and unchanged is removed, nested or not, and nothing is left under a private
/// name.
#[test]
fn removes_a_recorded_unchanged_file_leaving_nothing_behind() {
	let (dir, work) = work_dir("unchanged");
	std::fs::write(dir.join("a/b/f"), b"ours").unwrap();
	let identity = work.entry_identity(&path("a/b/f")).unwrap().unwrap();

	assert!(
		work
			.remove_file_if_identity(&path("a/b/f"), &identity)
			.unwrap()
	);

	assert!(names(&dir.join("a/b")).is_empty());
	assert!(
		!work
			.remove_file_if_identity(&path("a/b/f"), &identity)
			.unwrap()
	);
	std::fs::remove_dir_all(&dir).ok();
}

/// A file replaced or rewritten since it was recorded is left as it is.
#[test]
fn keeps_a_file_replaced_or_rewritten_since_it_was_recorded() {
	for rewrite in [false, true] {
		let (dir, work) = work_dir(if rewrite { "rewritten" } else { "replaced" });
		std::fs::write(dir.join("a/b/f"), b"ours").unwrap();
		let identity = work.entry_identity(&path("a/b/f")).unwrap().unwrap();
		if rewrite {
			std::fs::write(dir.join("a/b/f"), b"theirs!").unwrap();
		} else {
			std::fs::write(dir.join("a/b/prepared"), b"theirs!").unwrap();
			std::fs::rename(dir.join("a/b/prepared"), dir.join("a/b/f")).unwrap();
		}

		assert!(
			!work
				.remove_file_if_identity(&path("a/b/f"), &identity)
				.unwrap()
		);

		assert_eq!(std::fs::read(dir.join("a/b/f")).unwrap(), b"theirs!");
		assert_eq!(names(&dir.join("a/b")), ["f"]);
		std::fs::remove_dir_all(&dir).ok();
	}
}

/// Nothing at the name, or a file where one of its folders goes: no identity, nothing removed.
#[test]
fn finds_nothing_where_the_name_or_a_folder_is_missing() {
	let (dir, work) = work_dir("missing");
	std::fs::write(dir.join("a/b/f"), b"ours").unwrap();
	let identity = work.entry_identity(&path("a/b/f")).unwrap().unwrap();
	assert!(work.entry_identity(&path("a/b/g")).unwrap().is_none());
	assert!(work.entry_identity(&path("x/y")).unwrap().is_none());
	assert!(work.entry_identity(&path("a/b/f/z")).unwrap().is_none());
	std::fs::remove_dir_all(dir.join("a")).unwrap();
	std::fs::write(dir.join("a"), b"a file where the folder was").unwrap();

	assert!(
		!work
			.remove_file_if_identity(&path("a/b/f"), &identity)
			.unwrap()
	);

	assert_eq!(
		std::fs::read(dir.join("a")).unwrap(),
		b"a file where the folder was"
	);
	std::fs::remove_dir_all(&dir).ok();
}

/// An identity made from an `lstat` (a backend with none of its own) is honoured too: removed
/// while everything matches, kept once the file changed.
#[test]
fn honours_an_identity_made_from_an_lstat() {
	let (dir, work) = work_dir("from-meta");
	std::fs::write(dir.join("a/f"), b"ours").unwrap();
	std::fs::write(dir.join("a/g"), b"ours").unwrap();
	let f = WorkEntryIdentity::from_meta(&work.lstat(&path("a/f")).unwrap().unwrap());
	let g = WorkEntryIdentity::from_meta(&work.lstat(&path("a/g")).unwrap().unwrap());
	std::fs::write(dir.join("a/g"), b"theirs!").unwrap();

	assert!(work.remove_file_if_identity(&path("a/f"), &f).unwrap());
	assert!(!work.remove_file_if_identity(&path("a/g"), &g).unwrap());

	assert_eq!(names(&dir.join("a")), ["b", "g"]);
	std::fs::remove_dir_all(&dir).ok();
}

/// A file or symlink created and recorded is the entry its identity names: removed through it
/// while unchanged, kept once replaced; a name already taken creates nothing; no private names are
/// left behind.
#[test]
fn records_the_entry_it_creates() {
	let (dir, work) = work_dir("recorded");
	let file = work
		.populate_new_recorded(
			&path("a/b/f"),
			PopulationEntry::Regular {
				bytes: b"ours",
				executable: false,
			},
		)
		.unwrap()
		.unwrap();
	assert_eq!(std::fs::read(dir.join("a/b/f")).unwrap(), b"ours");
	assert_eq!(
		work.entry_identity(&path("a/b/f")).unwrap(),
		Some(file.clone())
	);
	#[cfg(unix)]
	{
		let link = work
			.populate_new_recorded(&path("a/b/l"), PopulationEntry::Symlink(b"f"))
			.unwrap()
			.unwrap();
		assert_eq!(
			std::fs::read_link(dir.join("a/b/l")).unwrap(),
			std::path::Path::new("f")
		);
		assert!(work.remove_file_if_identity(&path("a/b/l"), &link).unwrap());
	}
	assert!(
		work
			.populate_new_recorded(&path("a/b/f"), PopulationEntry::Symlink(b"x"))
			.unwrap()
			.is_none()
	);
	std::fs::write(dir.join("a/b/prepared"), b"theirs").unwrap();
	std::fs::rename(dir.join("a/b/prepared"), dir.join("a/b/f")).unwrap();

	assert!(!work.remove_file_if_identity(&path("a/b/f"), &file).unwrap());

	assert_eq!(std::fs::read(dir.join("a/b/f")).unwrap(), b"theirs");
	assert_eq!(names(&dir.join("a/b")), ["f"]);
	std::fs::remove_dir_all(&dir).ok();
}

/// A file recorded as created and then rewritten in place with contents of the same length, its
/// modification time put back (or within a coarse timestamp's tick): its metadata alone looks
/// unchanged, but the content written was recorded, so it's kept.
#[test]
fn keeps_a_same_length_rewrite_whose_time_was_put_back() {
	let (dir, work) = work_dir("same-length");
	let identity = work
		.populate_new_recorded(
			&path("a/f"),
			PopulationEntry::Regular {
				bytes: b"ours!",
				executable: false,
			},
		)
		.unwrap()
		.unwrap();
	let modified = std::fs::metadata(dir.join("a/f"))
		.unwrap()
		.modified()
		.unwrap();
	let file = std::fs::OpenOptions::new()
		.write(true)
		.open(dir.join("a/f"))
		.unwrap();
	std::io::Write::write_all(&mut &file, b"them!").unwrap();
	file.set_modified(modified).unwrap();
	drop(file);

	assert!(
		!work
			.remove_file_if_identity(&path("a/f"), &identity)
			.unwrap()
	);

	assert_eq!(std::fs::read(dir.join("a/f")).unwrap(), b"them!");
	assert_eq!(names(&dir.join("a")), ["b", "f"]);
	std::fs::remove_dir_all(&dir).ok();
}
