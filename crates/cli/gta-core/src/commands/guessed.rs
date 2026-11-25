//! git's warning when a commit it writes carries text it had to repair.
//!
//! A commit's text can't always be decoded by the encoding it declares (an encoding nothing here
//! knows, bytes invalid in it, or bytes that aren't UTF-8 with no `encoding` header). gitana then
//! reads it as git does when it writes: bytes that aren't UTF-8 read as Latin-1. The porcelain's
//! cherry-pick, revert and rebase report each commit they write that carried such repaired text
//! ([`gitana_porcelain::Written`]), and gta warns of each, as git does when it writes one.

/// git's warning, as it prints it to stderr.
const WARNING: &str = "Warning: commit message did not conform to UTF-8.\nYou may want to amend it \
                       after fixing the message, or set the config\nvariable i18n.commitEncoding \
                       to the encoding your project uses.";

/// Print git's warning for a commit written with repaired text.
pub(crate) fn warn_of_repair() {
	eprintln!("{WARNING}");
}

/// Print git's warning once for each of `commits` written with repaired text.
pub(crate) fn warn_of_repairs<T>(commits: &[T]) {
	for _ in commits {
		warn_of_repair();
	}
}
