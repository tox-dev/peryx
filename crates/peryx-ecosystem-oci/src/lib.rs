//! The OCI/Docker registry driver: the distribution-spec `/v2/` API served over peryx's
//! content-addressed blob store and metadata store.
//!
//! An OCI request is `/v2/<name>/(manifests|blobs|tags)/...`; `<name>` (which may contain slashes)
//! resolves to a configured `oci`-ecosystem index by longest route prefix, the same rule peryx
//! resolves any index route by. Blobs are `sha256`-addressed and map straight onto
//! [`peryx_storage::blob::BlobStorage`]; manifests are stored byte-for-byte so their digest is stable.

use std::collections::HashMap;
use std::sync::Arc;

use peryx_core::{Ecosystem, EcosystemInstaller, Lexicon};
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

/// The audience named by this registry's Bearer challenges and tokens.
pub const TOKEN_SERVICE: &str = "peryx";

mod discovery;
mod error;
mod mirror;
mod name;
pub mod openapi;
mod outbox;
mod quota;
pub(crate) mod registry;
mod search_oci;
mod settings;
mod store;
mod upstream;
mod web;

#[cfg(test)]
mod tests;

pub use error::{ErrorCode, error_response, gateway_error};
pub use mirror::{MirrorMode, MirrorRow, mirror};
pub use quota::quota_reservation;
pub use registry::OciRegistry;
#[doc(hidden)]
pub use registry::OciRegistryWithHasher;
pub use search_oci::OciIndexer;
pub use settings::{IndexSettings, LibraryPrefix};
pub use store::referenced_blob_digests;

#[derive(Debug)]
pub struct OciInstaller {
    settings: HashMap<String, IndexSettings>,
    journal_outbox: bool,
}

impl OciInstaller {
    pub fn new(settings: impl IntoIterator<Item = (String, IndexSettings)>, journal_outbox: bool) -> Self {
        Self {
            settings: settings.into_iter().collect(),
            journal_outbox,
        }
    }
}

impl EcosystemInstaller<AppState> for OciInstaller {
    fn register_driver(&self, state: &mut AppState) {
        if !state.indexes.iter().any(|index| index.ecosystem == Ecosystem::Oci) {
            return;
        }
        state.register_ecosystem(
            Arc::new(OciRegistry::new(
                self.settings.iter().map(|(name, settings)| (name.clone(), *settings)),
                self.journal_outbox,
            )),
            Arc::new(OciIndexer),
        );
        state.register_lexicon(Ecosystem::Oci, &OCI_LEXICON);
    }
}

/// Wire the OCI registry driver into a freshly built [`AppState`], with each OCI index's compiled
/// [`IndexSettings`] keyed by index name. An index absent from `settings` takes the defaults.
///
/// `journal_outbox` records each authoritative hosted mutation in the driver-transaction outbox for a
/// replica to reconcile; the composition root sets it from the resolved availability mode, so a
/// single-node `none` deployment records nothing extra.
///
/// Installs only when an `oci`-ecosystem index is configured: with none, the state keeps its no-op
/// driver and the `/v2/` namespace stays inert, so a deployment without OCI indexes carries no OCI cost.
pub fn install(
    state: &mut AppState,
    settings: impl IntoIterator<Item = (String, IndexSettings)>,
    journal_outbox: bool,
) {
    OciInstaller::new(settings, journal_outbox).install(state);
}
