//! The OCI/Docker registry driver: the distribution-spec `/v2/` API served over peryx's
//! content-addressed blob store and metadata store.
//!
//! An OCI request is `/v2/<name>/(manifests|blobs|tags)/...`; `<name>` (which may contain slashes)
//! resolves to a configured `oci`-ecosystem index by longest route prefix, the same rule peryx
//! resolves any index route by. Blobs are `sha256`-addressed and map straight onto
//! [`peryx_storage::blob::BlobStore`]; manifests are stored byte-for-byte so their digest is stable.

use std::sync::Arc;

use peryx_core::{Ecosystem, Lexicon};
use peryx_driver::AppState;

/// The container ecosystem's user-facing words for peryx's neutral concepts.
pub const OCI_LEXICON: Lexicon = Lexicon {
    server: "registry",
    collection: "repository",
    collections: "repositories",
    search_noun: "image",
    release: "tag",
    releases: "tags",
    artifact: "blob",
    artifacts: "blobs",
    get: "pull",
    put: "push",
};

mod discovery;
mod error;
mod mirror;
mod name;
pub mod openapi;
pub(crate) mod registry;
mod search_oci;
mod store;
mod upstream;

#[cfg(test)]
mod tests;

pub use error::{ErrorCode, error_response, gateway_error};
pub use mirror::{MirrorMode, MirrorRow, mirror};
pub use registry::OciRegistry;
pub use search_oci::OciIndexer;
pub use store::referenced_blob_digests;

/// Wire the OCI registry driver into a freshly built [`AppState`].
///
/// Installs only when an `oci`-ecosystem index is configured: with none, the state keeps its no-op
/// driver and the `/v2/` namespace stays inert, so a deployment without OCI indexes carries no OCI cost.
pub fn install(state: &mut AppState) {
    if state.indexes.iter().any(|index| index.ecosystem == Ecosystem::Oci) {
        state.register_namespace(Arc::new(OciRegistry::new()));
        state.add_search_indexer(Arc::new(OciIndexer));
        state.register_lexicon(Ecosystem::Oci, &OCI_LEXICON);
    }
}
