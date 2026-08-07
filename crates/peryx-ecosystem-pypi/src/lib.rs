//! The `PyPI` ecosystem driver for peryx: project names, versions, and the simple repository API.
//!
//! A future ecosystem is a sibling `peryx-ecosystem-*` crate, so nothing here is tangled into shared
//! code.

use std::sync::Arc;

use peryx_core::{Ecosystem, EcosystemInstaller, Lexicon};
use peryx_driver::AppState;
use peryx_driver::serving::{CompiledEcosystemSettings, EcosystemCapability, EcosystemDriver, EcosystemPlugin};

/// Stable identity of the Python package ecosystem.
pub const ECOSYSTEM: Ecosystem = Ecosystem::new("pypi");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorMode {
    All,
    Selected,
    MetadataOnly,
}

pub const DEFAULT_INDEXES: &[peryx_core::DefaultIndex] = &[
    peryx_core::DefaultIndex {
        name: "pypi",
        route: "pypi",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Cached {
            upstream: "https://pypi.org/simple/",
        },
    },
    peryx_core::DefaultIndex {
        name: "hosted",
        route: "hosted",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Hosted,
    },
    peryx_core::DefaultIndex {
        name: "root/pypi",
        route: "root/pypi",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Virtual {
            layers: &["hosted", "pypi"],
            upload: "hosted",
        },
    },
];

#[derive(Debug, Clone, Copy, Default)]
pub struct PypiPlugin;

#[cfg(feature = "serving")]
inventory::submit! {
    peryx_plugin_registry::PluginRegistration {
        plugin: &PypiPlugin,
        priority: 0,
    }
}

#[cfg(feature = "serving")]
mod admin;
#[cfg(feature = "serving")]
pub mod archive;
#[cfg(feature = "serving")]
pub mod attestation;
#[cfg(feature = "bench")]
pub mod bench;
#[cfg(feature = "serving")]
pub mod cache;
#[cfg(feature = "serving")]
pub mod catalog;
#[cfg(feature = "serving")]
mod catalog_job;
mod changelog;
#[cfg(feature = "serving")]
mod classifier;
#[cfg(feature = "serving")]
mod contact;
#[cfg(feature = "serving")]
mod description;
#[cfg(feature = "serving")]
pub mod discovery;
mod filename;
mod html;
#[cfg(feature = "serving")]
mod import;
mod legacy_json;
#[cfg(feature = "serving")]
mod license;
mod metadata;
#[cfg(feature = "serving")]
mod mirror;
mod name;
#[cfg(feature = "serving")]
pub mod openapi;
#[cfg(feature = "serving")]
pub mod policy;
#[cfg(feature = "serving")]
mod quota;
#[cfg(feature = "serving")]
mod requirement;
#[cfg(feature = "serving")]
pub mod retention;
#[cfg(feature = "serving")]
pub mod search_pypi;
mod serial;
#[cfg(feature = "serving")]
pub mod serving;
mod simple;
#[cfg(feature = "serving")]
mod simple_client;
#[cfg(feature = "serving")]
pub mod store;
#[cfg(feature = "serving")]
pub mod stream;
#[cfg(feature = "serving")]
mod sync_lock;
#[cfg(feature = "serving")]
pub mod trash;
#[cfg(feature = "serving")]
pub mod upload;
mod version;

#[cfg(feature = "serving")]
pub use quota::quota_reservation;
#[cfg(feature = "serving")]
pub use retention::evaluate_retention;
#[cfg(feature = "serving")]
pub use search_pypi::PypiIndexer;
#[cfg(feature = "serving")]
pub use serving::PypiServing;
#[cfg(feature = "serving")]
pub use serving::finalize::{
    Finalization, FinalizeDescriptor, FinalizeError, FinalizeFailure, finalize_admitted_upload,
};
#[cfg(feature = "serving")]
pub use simple_client::{ACCEPT_SIMPLE, SimpleClientExt, SimpleHead, SimpleResponse, UpstreamProtocol};

pub use changelog::{
    CHANGELOG_PAGE_SIZE, ChangelogEntry, ChangelogRequest, ChangelogRequestError, dispatch_changelog_request,
    parse_changelog_request, render_changelog_fault, render_changelog_response, render_last_serial_response,
};
pub use filename::{
    DistributionFilename, DistributionFilenameError, DistributionKind, distribution_name_segment,
    distribution_version_segment, parse_distribution_filename,
};
pub use html::{parse_detail_html, parse_index_html};
pub use legacy_json::render_legacy_json;
pub use metadata::{CoreMetadataDoc, MetadataError, parse_metadata, ui_meta, ui_project_from_detail};
pub use name::{
    PackageName, authority_key, file_matches_version, is_valid_name, normalize_name, normalize_name_cow,
    project_of_filename,
};
pub use serial::{
    ChangelogPage, ChangelogPageError, SerialStamp, UpstreamSerialError, compose_serial_watermarks,
    validate_upstream_serial,
};
pub use simple::{
    API_VERSION, API_VERSION_BASE, CoreMetadata, DetailSink, File, Meta, ParsedDetail, ProjectDetail, ProjectList,
    ProjectListEntry, ProjectStatus, Provenance, SimpleError, StreamedDetail, Yanked, parse_detail, parse_index,
    parse_meta, render_detail_html, render_index_html, stream_detail_json, to_json,
};
pub use version::{Version, VersionSpecifiers, parse_version, parse_version_specifiers, sorted_desc, versions_match};

/// Wire the `PyPI` serving driver and search indexer into a freshly built
/// [`AppState`](peryx_driver::ServingState).
///
/// [`AppState`](peryx_driver::ServingState) is ecosystem-neutral and starts with no-op serving/indexing
/// defaults; the composition root (the binary, and the serving tests) calls this once so requests
/// dispatch through [`PypiServing`] and search indexes through [`PypiIndexer`].
#[cfg(feature = "serving")]
pub fn install(state: &mut peryx_driver::AppState) {
    PypiServing.install(state);
}

#[cfg(feature = "serving")]
impl EcosystemInstaller<AppState> for PypiServing {
    fn register_driver(&self, state: &mut AppState) {
        let driver = Arc::new(*self);
        state.register_ecosystem(driver.clone(), Arc::new(PypiIndexer));
        state.register_maintenance_driver(ECOSYSTEM, driver.clone());
        state.register_replicated_apply_driver(ECOSYSTEM, driver);
        state.register_mirror_driver(ECOSYSTEM, Arc::new(Self));
        // peryx's neutral vocabulary is Python's own (index, project, version, file), so the PyPI
        // lexicon is the neutral one; a future divergence would give this crate its own constant.
        state.register_lexicon(ECOSYSTEM, &Lexicon::NEUTRAL);
    }
}

#[cfg(feature = "serving")]
impl EcosystemPlugin for PypiPlugin {
    fn ecosystem(&self) -> Ecosystem {
        ECOSYSTEM
    }

    fn default_indexes(&self) -> &'static [peryx_core::DefaultIndex] {
        DEFAULT_INDEXES
    }

    fn driver(&self) -> Arc<dyn EcosystemDriver> {
        Arc::new(PypiServing)
    }

    fn compile_index_settings(
        &self,
        name: &str,
        settings: &toml::Table,
    ) -> Result<Option<CompiledEcosystemSettings>, String> {
        settings.keys().next().map_or(Ok(None), |key| {
            Err(format!(
                "compile settings for {name}: unknown field `{key}` in `[index.settings]`"
            ))
        })
    }

    fn install(&self, state: &mut AppState, _: &[(&str, &CompiledEcosystemSettings)]) -> Result<(), String> {
        PypiServing.install(state);
        Ok(())
    }

    fn supports(&self, _capability: EcosystemCapability) -> bool {
        true
    }

    fn openapi_paths(&self, paths: utoipa::openapi::PathsBuilder) -> utoipa::openapi::PathsBuilder {
        openapi::openapi_paths(paths)
    }

    fn snippet_text(
        &self,
        base: &peryx_driver::discovery::BaseUrl,
        route: &str,
        uploads: bool,
        format: &str,
    ) -> Result<Option<String>, String> {
        let kind = match format {
            "pip.conf" => discovery::SnippetKind::PipConf,
            "uv.toml" => discovery::SnippetKind::UvToml,
            ".pypirc" => discovery::SnippetKind::Pypirc,
            _ => return Err(format!("unknown snippet format {format:?}")),
        };
        Ok(discovery::snippet_text(base, route, uploads, kind))
    }
}

#[cfg(test)]
mod plugin_contract_tests {
    use peryx_driver::discovery::BaseUrl;
    use peryx_driver::serving::{EcosystemCapability, EcosystemPlugin as _};

    use super::PypiPlugin;

    #[test]
    fn plugin_exposes_capabilities_and_validates_snippet_formats() {
        let plugin = PypiPlugin;
        let base = BaseUrl::parse("https://packages.example/").unwrap();

        assert!(plugin.supports(EcosystemCapability::CatalogSync));
        assert!(plugin.supports(EcosystemCapability::TrustedPublishing));
        assert!(plugin.snippet_text(&base, "pypi", true, "pip.conf").unwrap().is_some());
        assert!(plugin.snippet_text(&base, "pypi", true, "unknown").is_err());
    }
}

/// Render any error as the user-visible message a driver method returns, so the many `?`-adjacent
/// store and io failures map through one function instead of a per-site `|err| err.to_string()`
/// closure that never runs in the happy path.
#[cfg(feature = "serving")]
pub(crate) fn error_message<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

#[cfg(all(test, feature = "serving"))]
mod error_message_tests {
    use super::error_message;

    #[test]
    fn test_error_message_stringifies_io_and_store_faults() {
        assert_eq!(error_message(std::io::Error::other("disk")), "disk");
        let decode = serde_json::from_str::<u8>("x").unwrap_err();
        assert!(!error_message(peryx_storage::meta::MetaError::Decode(decode)).is_empty());
    }
}

#[cfg(test)]
mod tests;
