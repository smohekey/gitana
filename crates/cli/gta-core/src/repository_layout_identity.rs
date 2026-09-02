use gitana_fs_native::EntryIdentity;

/// Filesystem identities behind one discovered repository layout.
///
/// Paths and marker contents can be recreated while a command waits for repository serialization;
/// these identities bind the later capability reopen to the directories originally discovered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RepositoryLayoutIdentity {
	pub(crate) worktree: Option<EntryIdentity>,
	pub(crate) git: EntryIdentity,
	pub(crate) common: EntryIdentity,
}
