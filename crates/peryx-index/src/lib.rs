//! The index model and role engine for peryx.
//!
//! An index is a `(role, ecosystem, key)` triple. This crate owns the role axis: what a cached,
//! hosted, or virtual index *is*, how a request resolves to one, and the order a virtual index merges
//! its members in. It is ecosystem-neutral - it never parses a package format - and it sits below the
//! serving layer, so an ecosystem driver depends on the role engine rather than on the HTTP crate
//! that hosts it.

pub mod index;
pub mod registry;
pub mod resolve;
pub mod serving;

pub use index::{Index, IndexKind};
pub use registry::{IndexRegistry, IndexSet};
pub use resolve::{RouteResolver, layers_include_hosted, reaches_cached, remainder, shadow_order};
pub use serving::ServingCache;
