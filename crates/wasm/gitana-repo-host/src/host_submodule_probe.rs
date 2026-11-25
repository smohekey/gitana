use std::path::Path;

use anyhow::{Context as _, Result, bail};
use cap_std::{ambient_authority, fs::Dir};
use gitana_config::GitConfigSource;
use gitana_object::{HashAlgorithm, ObjectId, Sha1, Sha256};
use gitana_path::GitPath;
use gitana_submodule_probe::NativeSubmoduleProbe;
use gitana_worktree::{IgnoreSubmodules, SubmoduleProbe};

use crate::gitana::repo::submodule_types::IgnoreSubmodules as WitIgnoreSubmodules;
use crate::gitana::repo::submodules::SubmoduleReport as WitSubmoduleReport;

/// The host representation of the guest's `submodules.probe` resource: one status's look inside
/// the granted work tree's submodules, natively, under the repository's own object format.
pub struct HostSubmoduleProbe {
	/// The probe, or why the granted work tree couldn't be opened (every report then fails with
	/// it, as a native status would); `None` when nothing was granted.
	native: Option<Result<Native, String>>,
}

/// [`NativeSubmoduleProbe`] under the granted repository's object format.
enum Native {
	Sha1(NativeSubmoduleProbe<Sha1>),
	Sha256(NativeSubmoduleProbe<Sha256>),
}

impl HostSubmoduleProbe {
	/// A probe for the repository whose work tree is at `worktree`, or one with nothing to say
	/// when the host granted none.
	pub(crate) async fn new(worktree: Option<&Path>) -> Self {
		Self {
			native: match worktree {
				Some(worktree) => Some(open(worktree).await.map_err(|error| format!("{error:#}"))),
				None => None,
			},
		}
	}

	/// The `report` the guest asks for (see the WIT `submodules.probe`).
	pub(crate) async fn report(
		&self,
		path: Vec<u8>,
		recorded: String,
		flag: Option<WitIgnoreSubmodules>,
	) -> Result<Option<WitSubmoduleReport>, String> {
		let Some(native) = &self.native else {
			return Ok(None);
		};
		let path = GitPath::from_bytes(path).map_err(|error| error.to_string())?;
		let flag = flag.map(from_wit);
		match native.as_ref().map_err(Clone::clone)? {
			Native::Sha1(probe) => ask(probe, &path, &recorded, flag).await,
			Native::Sha256(probe) => ask(probe, &path, &recorded, flag).await,
		}
	}

	/// The warnings git would print, as the probe collected them.
	pub(crate) fn warnings(&self) -> Vec<String> {
		match &self.native {
			Some(Ok(Native::Sha1(probe))) => probe.warnings(),
			Some(Ok(Native::Sha256(probe))) => probe.warnings(),
			_ => Vec::new(),
		}
	}
}

async fn open(worktree: &Path) -> Result<Native> {
	let layout = gitana_repository_layout::inspect_root(worktree)
		.await
		.with_context(|| format!("{} isn't a git work tree", worktree.display()))?;
	let directory = |path: &Path| {
		Dir::open_ambient_dir(path, ambient_authority())
			.with_context(|| format!("couldn't open {}", path.display()))
	};
	let common = directory(&layout.common_dir)?;
	let git = directory(&layout.git_dir)?;
	// The object format is the repository's own, read from its config file alone, as git reads
	// `extensions.*`.
	let local = gitana_config_native::read_file_at(
		common.try_clone()?,
		Path::new("config"),
		&layout.common_dir.join("config"),
	)
	.await?;
	let local = GitConfigSource::parse(&String::from_utf8_lossy(&local.unwrap_or_default()))?;
	let format = local
		.get_string("extensions", None, "objectformat")
		.unwrap_or("sha1")
		.to_ascii_lowercase();
	let config =
		gitana_config_native::for_worktree_at(common, git, &layout.common_dir, &layout.git_dir).await?;
	Ok(match format.as_str() {
		"sha1" => Native::Sha1(NativeSubmoduleProbe::new(layout, config)),
		"sha256" => Native::Sha256(NativeSubmoduleProbe::new(layout, config)),
		other => bail!("the repository's object format, {other}, isn't one gitana reads"),
	})
}

async fn ask<H: HashAlgorithm>(
	probe: &NativeSubmoduleProbe<H>,
	path: &GitPath,
	recorded: &str,
	flag: Option<IgnoreSubmodules>,
) -> Result<Option<WitSubmoduleReport>, String> {
	let recorded = ObjectId::<H>::from_hex(recorded).map_err(|error| error.to_string())?;
	let report = probe
		.report(path, recorded, flag)
		.await
		.map_err(|error| error.to_string())?;
	Ok(report.map(|report| WitSubmoduleReport {
		ignore: into_wit(report.ignore),
		new_commits: report.new_commits,
		modified_content: report.modified_content,
		untracked_content: report.untracked_content,
	}))
}

fn into_wit(ignore: IgnoreSubmodules) -> WitIgnoreSubmodules {
	match ignore {
		IgnoreSubmodules::None => WitIgnoreSubmodules::None,
		IgnoreSubmodules::Untracked => WitIgnoreSubmodules::Untracked,
		IgnoreSubmodules::Dirty => WitIgnoreSubmodules::Dirty,
		IgnoreSubmodules::All => WitIgnoreSubmodules::All,
	}
}

fn from_wit(ignore: WitIgnoreSubmodules) -> IgnoreSubmodules {
	match ignore {
		WitIgnoreSubmodules::None => IgnoreSubmodules::None,
		WitIgnoreSubmodules::Untracked => IgnoreSubmodules::Untracked,
		WitIgnoreSubmodules::Dirty => IgnoreSubmodules::Dirty,
		WitIgnoreSubmodules::All => IgnoreSubmodules::All,
	}
}
