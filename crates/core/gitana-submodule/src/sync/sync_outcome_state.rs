/// What URL synchronization did for one selected submodule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncOutcomeState {
	SkippedUnregistered,
	SkippedInactive,
	SkippedConflicted,
	Synchronized,
}
