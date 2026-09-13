/// The update strategy explicitly selected by a caller.
///
/// An absent override lets repository configuration or `.gitmodules` select the strategy. `none`
/// remains a configured skip rather than a command-line strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateStrategy {
	Checkout,
	Merge,
}
