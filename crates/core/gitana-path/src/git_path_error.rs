use thiserror::Error;

/// A byte sequence is not a safe canonical repository-relative Git path.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GitPathError {
	/// Git path names cannot contain NUL bytes.
	#[error("Git paths cannot contain NUL bytes")]
	Nul,
	/// A full repository path cannot begin or end with `/` or contain an empty component.
	#[error("Git paths must be relative and cannot contain empty components")]
	EmptyComponent,
	/// `.` and `..` are not repository path components.
	#[error("Git paths cannot contain `.` or `..` components")]
	Traversal,
	/// A tree entry name must contain exactly one non-empty component.
	#[error("Git tree entry names must contain exactly one component")]
	NotComponent,
}
