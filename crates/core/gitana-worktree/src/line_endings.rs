//! Line-ending conversion between the working tree and the repository, as git's `convert.c` does
//! it for the `text`, `crlf` and `eol` attributes and `core.autocrlf`, `core.eol` and
//! `core.safecrlf`.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::{AttrState, CrlfAction, EolConfig, LineEndingChange, SafeCrlf, TextStats};

/// What git's `text` (or the older `crlf`) attribute declares, before `eol` and config settle it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Declared {
	Undefined,
	Binary,
	Text,
	TextInput,
	Auto,
}

/// git's `convert_attrs`: what the `text`, `crlf` and `eol` attributes, then `core.autocrlf` and
/// `core.eol`, make of a file's line endings.
pub(crate) fn crlf_action(attrs: &HashMap<Vec<u8>, AttrState>, config: &EolConfig) -> CrlfAction {
	let declared = |name: &[u8]| match attrs.get(name) {
		Some(AttrState::Set) => Declared::Text,
		Some(AttrState::Unset) => Declared::Binary,
		Some(AttrState::Value(value)) if value == b"input" => Declared::TextInput,
		Some(AttrState::Value(value)) if value == b"auto" => Declared::Auto,
		_ => Declared::Undefined,
	};
	let mut action = declared(b"text");
	if action == Declared::Undefined {
		action = declared(b"crlf");
	}
	let crlf = config.text_eol_is_crlf();
	if action != Declared::Binary {
		match attrs.get(b"eol".as_slice()) {
			Some(AttrState::Value(value)) if value == b"lf" => {
				return if action == Declared::Auto {
					CrlfAction::AutoInput
				} else {
					CrlfAction::TextInput
				};
			}
			Some(AttrState::Value(value)) if value == b"crlf" => {
				return if action == Declared::Auto {
					CrlfAction::AutoCrlf
				} else {
					CrlfAction::TextCrlf
				};
			}
			_ => {}
		}
	}
	match action {
		Declared::Binary => CrlfAction::Binary,
		Declared::TextInput => CrlfAction::TextInput,
		Declared::Text if crlf => CrlfAction::TextCrlf,
		Declared::Text => CrlfAction::TextInput,
		Declared::Auto if crlf => CrlfAction::AutoCrlf,
		Declared::Auto => CrlfAction::AutoInput,
		Declared::Undefined => match config.autocrlf {
			crate::AutoCrlf::False => CrlfAction::Binary,
			crate::AutoCrlf::True => CrlfAction::AutoCrlf,
			crate::AutoCrlf::Input => CrlfAction::AutoInput,
		},
	}
}

fn is_auto(action: CrlfAction) -> bool {
	matches!(action, CrlfAction::AutoInput | CrlfAction::AutoCrlf)
}

fn writes_crlf(action: CrlfAction) -> bool {
	matches!(action, CrlfAction::TextCrlf | CrlfAction::AutoCrlf)
}

/// git's `will_convert_lf_to_crlf`.
fn will_convert_lf_to_crlf(stats: &TextStats, action: CrlfAction) -> bool {
	if !writes_crlf(action) || stats.lonelf == 0 {
		return false;
	}
	// Auto leaves a file that has any CR, or looks binary, as it is.
	!is_auto(action) || (stats.lonecr == 0 && stats.crlf == 0 && !stats.is_binary())
}

/// Whether staging `content` under `action` depends on the index's copy having CRLF (git's
/// `has_crlf_in_index`), so the caller need read that copy only then.
pub(crate) fn needs_index_check(content: &[u8], action: CrlfAction) -> bool {
	if !is_auto(action) || content.is_empty() {
		return false;
	}
	let stats = TextStats::of(content);
	stats.crlf > 0 && !stats.is_binary()
}

/// git's check of the index's copy for "auto": whether `blob` has CRLF and isn't binary.
pub(crate) fn blob_has_crlf(blob: &[u8]) -> bool {
	if !blob.contains(&b'\r') {
		return false;
	}
	let stats = TextStats::of(blob);
	stats.crlf > 0 && !stats.is_binary()
}

/// git's `crlf_to_git`: `content`, a working-tree file, as the repository stores it.
/// `index_has_crlf` is [`blob_has_crlf`] of the index's copy (only asked when
/// [`needs_index_check`]). `safecrlf` is given when staging, as git's `add` checks: then the
/// change git warns of is returned with the content, or, for `core.safecrlf=true`, the error.
pub(crate) fn to_git<'a>(
	content: &'a [u8],
	action: CrlfAction,
	index_has_crlf: bool,
	safecrlf: Option<SafeCrlf>,
) -> Result<(Cow<'a, [u8]>, Option<LineEndingChange>), LineEndingChange> {
	if action == CrlfAction::Binary || content.is_empty() {
		return Ok((Cow::Borrowed(content), None));
	}
	let stats = TextStats::of(content);
	let mut convert = stats.crlf > 0;
	if is_auto(action) {
		if stats.is_binary() {
			return Ok((Cow::Borrowed(content), None));
		}
		// git's safer autocrlf: a file the index holds with CRLF keeps its CRLF.
		if index_has_crlf {
			convert = false;
		}
	}
	let mut change = None;
	if let Some(safecrlf) = safecrlf.filter(|safecrlf| *safecrlf != SafeCrlf::False) {
		// What a checkout would make of what's stored.
		let mut after = stats;
		if convert {
			after.lonelf += after.crlf;
			after.crlf = 0;
		}
		if will_convert_lf_to_crlf(&after, action) {
			after.crlf += after.lonelf;
			after.lonelf = 0;
		}
		let found = if stats.crlf > 0 && after.crlf == 0 {
			Some(LineEndingChange::CrlfToLf)
		} else if stats.lonelf > 0 && after.lonelf == 0 {
			Some(LineEndingChange::LfToCrlf)
		} else {
			None
		};
		if let Some(found) = found {
			if safecrlf == SafeCrlf::True {
				return Err(found);
			}
			change = Some(found);
		}
	}
	if !convert {
		return Ok((Cow::Borrowed(content), change));
	}
	let mut stored = Vec::with_capacity(content.len());
	let mut index = 0;
	while index < content.len() {
		let byte = content[index];
		index += 1;
		if byte == b'\r' && content.get(index) == Some(&b'\n') {
			continue;
		}
		stored.push(byte);
	}
	Ok((Cow::Owned(stored), change))
}

/// git's `crlf_to_worktree`: `blob`, as stored, as it's written into the working tree.
pub(crate) fn to_worktree(blob: &[u8], action: CrlfAction) -> Cow<'_, [u8]> {
	if blob.is_empty() || !writes_crlf(action) {
		return Cow::Borrowed(blob);
	}
	let stats = TextStats::of(blob);
	if !will_convert_lf_to_crlf(&stats, action) {
		return Cow::Borrowed(blob);
	}
	let mut written = Vec::with_capacity(blob.len() + stats.lonelf);
	for (index, byte) in blob.iter().enumerate() {
		if *byte == b'\n' && (index == 0 || blob[index - 1] != b'\r') {
			written.push(b'\r');
		}
		written.push(*byte);
	}
	Cow::Owned(written)
}

#[cfg(all(test, unix))]
mod tests {
	use std::path::Path;
	use std::process::{Command, Output};

	use super::*;
	use crate::AttrFile;

	fn run(dir: &Path, config: &[&str], args: &[&str]) -> Output {
		let mut command = Command::new("git");
		command
			.arg("-C")
			.arg(dir)
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_ATTR_NOSYSTEM", "1");
		for setting in config {
			command.args(["-c", setting]);
		}
		command.args(args).output().unwrap()
	}

	fn git(dir: &Path, config: &[&str], args: &[&str]) -> Vec<u8> {
		let out = run(dir, config, args);
		assert!(
			out.status.success(),
			"git {args:?}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		out.stdout
	}

	const CONTENTS: [(&str, &[u8]); 8] = [
		("lf", b"one\ntwo\n"),
		("crlf", b"one\r\ntwo\r\n"),
		("mixed", b"one\r\ntwo\nthree\r\n"),
		("lonecr", b"one\rtwo\n"),
		("binary", b"one\0two\r\n"),
		("empty", b""),
		("bare", b"no newline"),
		("eof", b"one\r\ntwo\r\n\x1a"),
	];
	const ATTRS: [&str; 12] = [
		"",
		"text",
		"-text",
		"text=auto",
		"eol=crlf",
		"eol=lf",
		"text eol=lf",
		"text=auto eol=crlf",
		"crlf",
		"-crlf",
		"crlf=input",
		"binary",
	];
	const CONFIGS: [&[&str]; 6] = [
		&[],
		&["core.autocrlf=true"],
		&["core.autocrlf=input"],
		&["core.eol=crlf"],
		&["core.eol=lf"],
		&["core.autocrlf=false", "core.eol=native"],
	];

	fn config_of(dir: &Path, settings: &[&str]) -> EolConfig {
		let mut text = String::new();
		for setting in settings {
			let (key, value) = setting.split_once('=').unwrap();
			let name = key.strip_prefix("core.").unwrap();
			text.push_str(&format!("[core]\n\t{name} = {value}\n"));
		}
		let _ = dir;
		EolConfig::read(&gitana_config::GitConfig::parse(&text).unwrap()).unwrap()
	}

	fn action_of(attr_line: &str, config: &EolConfig) -> CrlfAction {
		let file = AttrFile::parse(format!("f.* {attr_line}\n").as_bytes(), b"", true);
		let attrs = crate::attributes::resolve(&[&file], b"f.x", false);
		crlf_action(&attrs, config)
	}

	/// What gitana stages and writes matches what git's `add` stores and its checkout writes, for
	/// every content, attribute line and config in the matrix.
	#[test]
	fn converts_as_git_add_and_checkout_do() {
		for settings in CONFIGS {
			for attr_line in ATTRS {
				let temp = tempfile::tempdir().unwrap();
				let work = temp.path();
				git(work, &[], &["init", "-q"]);
				std::fs::write(work.join(".gitattributes"), format!("f.* {attr_line}\n")).unwrap();
				let config = config_of(work, settings);
				let action = action_of(attr_line, &config);
				for (name, content) in CONTENTS {
					let file = format!("f.{name}");
					std::fs::write(work.join(&file), content).unwrap();
					git(work, settings, &["add", "--", &file]);
					let stored = git(work, settings, &["cat-file", "-p", &format!(":{file}")]);
					let (ours, _) = to_git(content, action, false, None).unwrap();
					assert_eq!(
						ours.as_ref(),
						stored.as_slice(),
						"stored {file} with {settings:?} `{attr_line}`"
					);
					// Written out: git checks out the stored blob.
					std::fs::remove_file(work.join(&file)).unwrap();
					git(work, settings, &["checkout", "--", &file]);
					let written = std::fs::read(work.join(&file)).unwrap();
					assert_eq!(
						to_worktree(&stored, action).as_ref(),
						written.as_slice(),
						"written {file} with {settings:?} `{attr_line}`"
					);
				}
			}
		}
	}

	/// The change `core.safecrlf=warn` warns of, and `true` refuses, match git's `add`.
	#[test]
	fn warns_and_refuses_as_git_safecrlf_does() {
		for settings in CONFIGS {
			for attr_line in ATTRS {
				let temp = tempfile::tempdir().unwrap();
				let work = temp.path();
				git(work, &[], &["init", "-q"]);
				std::fs::write(work.join(".gitattributes"), format!("f.* {attr_line}\n")).unwrap();
				let config = config_of(work, settings);
				let action = action_of(attr_line, &config);
				for (name, content) in CONTENTS {
					let file = format!("f.{name}");
					std::fs::write(work.join(&file), content).unwrap();
					let mut warn = settings.to_vec();
					warn.push("core.safecrlf=warn");
					let out = run(work, &warn, &["add", "--", &file]);
					let stderr = String::from_utf8_lossy(&out.stderr);
					let git_warned = if stderr.contains("CRLF will be replaced by LF") {
						Some(LineEndingChange::CrlfToLf)
					} else if stderr.contains("LF will be replaced by CRLF") {
						Some(LineEndingChange::LfToCrlf)
					} else {
						None
					};
					let (_, ours) = to_git(content, action, false, Some(SafeCrlf::Warn)).unwrap();
					assert_eq!(
						ours, git_warned,
						"warned {file} with {settings:?} `{attr_line}`"
					);
					git(work, &[], &["rm", "-q", "--cached", "-f", "--", &file]);
					let mut strict = settings.to_vec();
					strict.push("core.safecrlf=true");
					let out = run(work, &strict, &["add", "--", &file]);
					let git_refused = !out.status.success();
					let ours = to_git(content, action, false, Some(SafeCrlf::True)).is_err();
					assert_eq!(
						ours, git_refused,
						"refused {file} with {settings:?} `{attr_line}`"
					);
					let _ = run(work, &[], &["rm", "-q", "--cached", "-f", "--", &file]);
				}
			}
		}
	}

	/// With "auto", a file the index already holds with CRLF keeps its CRLF when staged again, as
	/// git's safer autocrlf does.
	#[test]
	fn auto_keeps_crlf_the_index_already_holds_as_git_does() {
		let temp = tempfile::tempdir().unwrap();
		let work = temp.path();
		git(work, &[], &["init", "-q"]);
		std::fs::write(work.join("f.txt"), b"one\r\ntwo\r\n").unwrap();
		git(work, &[], &["add", "f.txt"]);
		std::fs::write(work.join(".gitattributes"), "*.txt text=auto\n").unwrap();
		let edited = b"one\r\ntwo\r\nthree\r\n";
		std::fs::write(work.join("f.txt"), edited).unwrap();
		git(work, &[], &["add", "f.txt"]);
		let stored = git(work, &[], &["cat-file", "-p", ":f.txt"]);
		let config = config_of(work, &[]);
		let action = action_of("text=auto", &config);
		assert!(needs_index_check(edited, action));
		let (ours, _) = to_git(edited, action, blob_has_crlf(b"one\r\ntwo\r\n"), None).unwrap();
		assert_eq!(ours.as_ref(), stored.as_slice());
	}
}
