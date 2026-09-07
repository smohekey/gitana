/// Result of editing a submodule branch declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetBranchOutcome {
	/// The requested branch state was applied to the in-memory configuration.
	Applied,
	/// The declaration already used its default remote branch.
	AlreadyDefault,
}
