/// Which way content is crossing, as git's attribute direction: from the working tree into the
/// repository (staging, and comparing a file with the index), or out to the working tree.
/// It decides where `.gitattributes` are read first, and which filter driver would run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttrDirection {
	/// The working tree's `.gitattributes` first, the index's as the fallback.
	CheckIn,
	/// The index's first (the one the checkout makes), the working tree's as the fallback.
	CheckOut,
}
