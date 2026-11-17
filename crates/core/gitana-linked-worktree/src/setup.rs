use gitana_config::GitConfig;
use gitana_worktree::OutsideAttributes;

/// What the caller knows of the user's git setup beyond the repository, for the operations that
/// check files out or compare them with the index: the merged config (system, global, the
/// repository's own and the command line's), and the attributes files from outside the repository.
#[derive(Clone, Debug, Default)]
pub struct Setup {
	/// The merged config, as the caller sees it; `None` reads the repository's own config alone.
	pub config: Option<GitConfig>,
	/// The system and global attributes files.
	pub attributes: OutsideAttributes,
}
