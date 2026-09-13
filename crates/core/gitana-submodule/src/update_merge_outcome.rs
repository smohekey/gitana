use crate::SubmoduleObjectId;

/// The successful result of merging a selected update target into an existing module checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateMergeOutcome {
	AlreadyUpToDate,
	FastForward {
		from: Option<SubmoduleObjectId>,
		to: SubmoduleObjectId,
	},
	Made {
		commit: SubmoduleObjectId,
	},
}
