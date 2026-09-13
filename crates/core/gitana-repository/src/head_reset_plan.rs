use gitana_object::{HashAlgorithm, ObjectId};

#[derive(Clone)]
pub(crate) struct HeadResetPlan<H: HashAlgorithm> {
	pub(crate) target: ObjectId<H>,
	pub(crate) orig_head: Option<ObjectId<H>>,
	pub(crate) committer: Option<String>,
	pub(crate) message: Option<String>,
}
