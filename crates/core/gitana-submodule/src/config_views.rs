use gitana_config::GitConfig;

/// Configuration layers with deliberately different ownership for a submodule operation.
#[derive(Clone)]
pub struct ConfigViews {
	/// Effective superproject config, including local/worktree and command-line overrides.
	pub superproject: GitConfig,
}

impl ConfigViews {
	pub fn new(superproject: GitConfig) -> Self {
		Self { superproject }
	}
}
