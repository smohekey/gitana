//! The host's `submodules` capability, as the [`SubmoduleProbe`] the working tree's status asks.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;

use gitana_object::{HashAlgorithm, ObjectId};
use gitana_path::GitPath;
use gitana_worktree::{IgnoreSubmodules, SubmoduleProbe, SubmoduleReport};

use crate::bindings::gitana::repo::submodule_types::IgnoreSubmodules as WitIgnoreSubmodules;
use crate::bindings::gitana::repo::submodules::Probe;

/// One status's look inside the submodules, answered by the host: a `submodules.probe` made for
/// that status.
pub(crate) struct HostSubmodules {
	probe: Probe,
}

impl HostSubmodules {
	/// A probe for one status.
	pub(crate) fn new() -> Self {
		Self {
			probe: Probe::new(),
		}
	}
}

impl<H: HashAlgorithm> SubmoduleProbe<H> for HostSubmodules {
	fn report<'a>(
		&'a self,
		path: &'a GitPath,
		recorded: ObjectId<H>,
		flag: Option<IgnoreSubmodules>,
	) -> Pin<
		Box<
			dyn Future<Output = Result<Option<SubmoduleReport>, Box<dyn Error + Send + Sync>>>
				+ Send
				+ 'a,
		>,
	> {
		// The host answers synchronously, as the component's other imports do.
		let answer = self
			.probe
			.report(path.as_bytes(), &recorded.to_hex(), flag.map(into_wit))
			.map(|report| {
				report.map(|report| SubmoduleReport {
					ignore: from_wit(report.ignore),
					new_commits: report.new_commits,
					modified_content: report.modified_content,
					untracked_content: report.untracked_content,
				})
			})
			.map_err(Into::into);
		Box::pin(async move { answer })
	}
}

pub(crate) fn into_wit(ignore: IgnoreSubmodules) -> WitIgnoreSubmodules {
	match ignore {
		IgnoreSubmodules::None => WitIgnoreSubmodules::None,
		IgnoreSubmodules::Untracked => WitIgnoreSubmodules::Untracked,
		IgnoreSubmodules::Dirty => WitIgnoreSubmodules::Dirty,
		IgnoreSubmodules::All => WitIgnoreSubmodules::All,
	}
}

pub(crate) fn from_wit(ignore: WitIgnoreSubmodules) -> IgnoreSubmodules {
	match ignore {
		WitIgnoreSubmodules::None => IgnoreSubmodules::None,
		WitIgnoreSubmodules::Untracked => IgnoreSubmodules::Untracked,
		WitIgnoreSubmodules::Dirty => IgnoreSubmodules::Dirty,
		WitIgnoreSubmodules::All => IgnoreSubmodules::All,
	}
}
