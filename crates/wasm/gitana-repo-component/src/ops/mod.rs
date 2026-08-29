//! Generic operation bodies, written once over the hash algorithm `H` and driven by
//! [`crate::inner::Inner`]'s dispatch. One module per op domain; conversions to the
//! WIT types happen here, at the boundary.

mod error;
mod git_path;
mod host_identity;
mod include_resolver;
mod objects;
mod refs;
mod remote;
mod repo;
mod revisions;
mod wasi_credentials;
mod worktree;

pub(crate) use self::{
	error::{repo_error, worktree_error},
	git_path::{
		display_path, display_revision, from_wit as git_path_from_wit, into_wit as git_path_into_wit,
		into_worktree_text, revision_from_wit, tree_path_into_wit,
	},
	host_identity::HostIdentity,
	include_resolver::FileStoreIncludeResolver,
	objects::{
		create_commit, ls_tree, read_blob, read_commit, read_object, read_tag, write_blob, write_tree,
	},
	refs::{
		delete_ref, head, list_refs, read_symbolic_ref, resolve_ref, set_symbolic_ref, update_ref,
	},
	remote::{
		auth_transport, clone, clone_negotiate, clone_ssh, fetch, open_ssh_clone, parse_remote_url,
		push,
	},
	repo::{init_layout, init_repo, install_effective_config, read_config, repack},
	revisions::{is_ancestor, merge_base, rev_list, rev_parse},
	wasi_credentials::WasiCredentialProvider,
	worktree::{
		add, checkout, commit, sparse_add, sparse_disable, sparse_list, sparse_reapply, sparse_set,
		status,
	},
};
