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
use peryx_driver::serving::{
    CompiledEcosystemSettings, EcosystemDriver, EcosystemPlugin, MirrorAction, MirrorDriver, MirrorRequest,
};

/// Stable identity of the OCI distribution ecosystem.
pub const ECOSYSTEM: Ecosystem = Ecosystem::new("oci");

pub const DEFAULT_INDEXES: &[peryx_core::DefaultIndex] = &[
    peryx_core::DefaultIndex {
        name: "dockerhub",
        route: "dockerhub",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Cached {
            upstream: "https://registry-1.docker.io",
        },
    },
    peryx_core::DefaultIndex {
        name: "images",
        route: "images",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Hosted,
    },
    peryx_core::DefaultIndex {
        name: "root/oci",
        route: "root/oci",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Virtual {
            layers: &["images", "dockerhub"],
            upload: "images",
        },
    },
];

#[derive(Debug, Clone, Copy, Default)]
pub struct OciPlugin;

inventory::submit! {
    peryx_plugin_registry::PluginRegistration {
        plugin: &OciPlugin,
        priority: 1,
    }
}

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
pub const TOKEN_SERVICE: &str = peryx_identity::TOKEN_AUDIENCE;

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
        if !state.indexes.iter().any(|index| index.ecosystem == ECOSYSTEM) {
            return;
        }
        let driver = Arc::new(OciRegistry::new(
            self.settings.iter().map(|(name, settings)| (name.clone(), *settings)),
            self.journal_outbox,
        ));
        state.register_ecosystem(driver.clone(), Arc::new(OciIndexer));
        state.register_maintenance_driver(ECOSYSTEM, driver.clone());
        state.register_mirror_driver(ECOSYSTEM, driver);
        state.register_lexicon(ECOSYSTEM, &OCI_LEXICON);
    }
}

impl EcosystemPlugin for OciPlugin {
    fn ecosystem(&self) -> Ecosystem {
        ECOSYSTEM
    }

    fn default_indexes(&self) -> &'static [peryx_core::DefaultIndex] {
        DEFAULT_INDEXES
    }

    fn driver(&self) -> Arc<dyn EcosystemDriver> {
        Arc::new(OciRegistry::default())
    }

    fn compile_index_settings(
        &self,
        name: &str,
        settings: &toml::Table,
    ) -> Result<Option<CompiledEcosystemSettings>, String> {
        IndexSettings::compile(settings)
            .map(|settings| Some(CompiledEcosystemSettings::new(ECOSYSTEM, settings)))
            .map_err(|reason| format!("compile settings for {name}: {reason}"))
    }

    fn install(&self, state: &mut AppState, settings: &[(&str, &CompiledEcosystemSettings)]) -> Result<(), String> {
        install_compiled(state, settings, false)
    }

    fn install_distributed(
        &self,
        state: &mut AppState,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        install_compiled(state, settings, true)
    }

    fn openapi_paths(&self, paths: utoipa::openapi::PathsBuilder) -> utoipa::openapi::PathsBuilder {
        openapi::openapi_paths(paths)
    }
}

fn install_compiled(
    state: &mut AppState,
    settings: &[(&str, &CompiledEcosystemSettings)],
    journal_outbox: bool,
) -> Result<(), String> {
    let settings = settings
        .iter()
        .map(|(name, settings)| {
            settings
                .value::<IndexSettings>()
                .copied()
                .map(|settings| ((*name).to_owned(), settings))
                .ok_or_else(|| format!("compiled settings for {name} have the wrong type"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    OciInstaller::new(settings, journal_outbox).install(state);
    Ok(())
}

#[async_trait::async_trait]
impl<S: std::hash::BuildHasher + Send + Sync> MirrorDriver for registry::OciRegistryWithHasher<S> {
    async fn mirror(
        &self,
        state: Arc<AppState>,
        request: MirrorRequest<'_>,
        output: &mut (dyn std::io::Write + Send),
    ) -> Result<(), String> {
        let index = state
            .indexes
            .iter()
            .find(|index| index.name == request.index || index.route == request.index)
            .ok_or_else(|| format!("unknown OCI index {:?}", request.index))?;
        let mut images = table_strings(request.configured, "images")?;
        if images.is_empty() {
            images = table_strings(request.configured, "packages")?;
        }
        images.extend(table_strings(request.overrides, "images")?);
        if images.is_empty() {
            return Err(
                "mirroring an OCI index needs at least one image (--image or [index.prefetch] packages)".to_owned(),
            );
        }
        output
            .write_all(b"kind\tindex\tproject\tfilename\tdigest\turl\tbytes\tstatus\treason\n")
            .map_err(|error| error.to_string())?;
        if request.action == MirrorAction::Plan {
            for image in &images {
                writeln!(output, "manifest\t{}\t{image}\t{image}\t\t\t0\tselected\t", index.name)
                    .map_err(|error| error.to_string())?;
            }
            writeln!(
                output,
                "summary\t{}\t\timages\t\t\t{}\timages\t",
                index.name,
                images.len()
            )
            .map_err(|error| error.to_string())?;
            return Ok(());
        }
        let settings = IndexSettings::compile(request.settings)?;
        let mode = match request.action {
            MirrorAction::Sync => MirrorMode::Sync,
            MirrorAction::Verify => MirrorMode::Verify,
            MirrorAction::Plan => unreachable!(),
        };
        let rows = mirror(&state.serving, index, settings, &images, mode)
            .await
            .map_err(|error| error.to_string())?;
        let mut errors = 0_u64;
        for row in rows {
            errors += u64::from(row.status == "error");
            writeln!(
                output,
                "{}\t{}\t{}\t{}\t{}\t\t{}\t{}\t{}",
                row.kind, row.repo, row.reference, row.reference, row.digest, row.bytes, row.status, row.reason
            )
            .map_err(|error| error.to_string())?;
        }
        if errors == 0 {
            Ok(())
        } else {
            Err(format!("mirror found {errors} error(s)"))
        }
    }
}

fn table_strings(table: &toml::Table, key: &str) -> Result<Vec<String>, String> {
    table.get(key).map_or(Ok(Vec::new()), |value| {
        value
            .as_array()
            .ok_or_else(|| format!("{key} must be an array"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{key} entries must be strings"))
            })
            .collect()
    })
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
#[cfg(feature = "bench")]
pub mod bench;
mod upload_session;
