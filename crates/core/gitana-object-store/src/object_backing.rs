/// The physical files from which one verified Git object was read.
///
/// Callers that publish a durable revision use this provenance to flush the exact loose object or
/// pack files that made the object readable, then re-read it to detect a concurrent repack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectBacking {
	/// A zlib-compressed loose object.
	Loose {
		/// Repository-relative loose-object path.
		path: String,
	},
	/// An object materialised from a pack and an optional index sidecar.
	Packed {
		/// Repository-relative `.pack` path.
		pack: String,
		/// Repository-relative `.idx` path when one backed this read.
		///
		/// Gitana can rebuild an index in memory for a legacy or foreign pack that has no sidecar.
		index: Option<String>,
	},
}

impl ObjectBacking {
	/// Visit every regular file that must survive for this backing to remain readable.
	pub fn files(&self) -> impl Iterator<Item = &str> {
		let (first, second) = match self {
			Self::Loose { path } => (path.as_str(), None),
			Self::Packed { pack, index } => (pack.as_str(), index.as_deref()),
		};
		std::iter::once(first).chain(second)
	}
}
