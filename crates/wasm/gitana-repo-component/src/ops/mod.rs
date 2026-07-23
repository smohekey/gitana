//! Generic operation bodies, written once over the hash algorithm `H` and driven by
//! [`crate::inner::Inner`]'s dispatch. One module per op domain; conversions to the
//! WIT types happen here, at the boundary.

mod error;
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
	worktree::{add, checkout, commit, status},
};
