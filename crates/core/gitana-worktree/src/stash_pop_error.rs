use gitana_object::HashAlgorithm;
use gitana_repository::RepositoryError;

use crate::{StashApplied, StashApplyError};

/// Why [`WorkTree::stash_pop`](crate::WorkTree::stash_pop) failed.
#[derive(Debug, thiserror::Error)]
pub enum StashPopError<H: HashAlgorithm> {
	/// Nothing was brought back, as [`StashApplyError`] says; the stash is kept.
	#[error(transparent)]
	Apply(#[from] StashApplyError),
	/// The stash applied cleanly but couldn't be dropped; it's kept.
	#[error("could not drop the stash: {error}")]
	Drop {
		applied: StashApplied<H>,
		error: RepositoryError,
	},
}
