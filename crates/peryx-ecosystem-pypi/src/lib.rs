//! The `PyPI` ecosystem driver for peryx: project names, versions, and the simple repository API.
//!
//! A future ecosystem is a sibling `peryx-ecosystem-*` crate, so nothing here is tangled into shared
//! code.

#[cfg(feature = "serving")]
mod admin;
#[cfg(feature = "serving")]
pub mod archive;
#[cfg(feature = "serving")]
pub mod cache;
#[cfg(feature = "serving")]
pub mod discovery;
mod filename;
mod html;
#[cfg(feature = "serving")]
mod import;
mod legacy_json;
mod metadata;
mod name;
#[cfg(feature = "serving")]
pub mod openapi;
#[cfg(feature = "serving")]
pub mod policy;
#[cfg(feature = "serving")]
pub mod search_pypi;
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
pub mod upload;
mod version;

#[cfg(feature = "serving")]
pub use search_pypi::PypiIndexer;
#[cfg(feature = "serving")]
pub use serving::PypiServing;
#[cfg(feature = "serving")]
pub use simple_client::{ACCEPT_SIMPLE, SimpleClientExt, SimpleHead, SimpleResponse, UpstreamProtocol};

pub use filename::{
    DistributionFilename, DistributionFilenameError, DistributionKind, distribution_version_segment,
    parse_distribution_filename,
};
pub use html::{parse_detail_html, parse_index_html};
pub use legacy_json::render_legacy_json;
pub use metadata::{CoreMetadataDoc, parse_metadata, ui_meta, ui_project_from_detail};
pub use name::{
    PackageName, file_matches_version, is_valid_name, normalize_name, normalize_name_cow, project_of_filename,
};
pub use simple::{
    API_VERSION, CoreMetadata, File, Meta, ParsedDetail, ProjectDetail, ProjectList, ProjectListEntry, ProjectStatus,
    Provenance, SimpleError, Yanked, parse_detail, parse_index, parse_meta, render_detail_html, render_index_html,
    to_json,
};
pub use version::{Version, VersionSpecifiers, parse_version, parse_version_specifiers, sorted_desc};

/// Wire the `PyPI` serving driver and search indexer into a freshly built
/// [`AppState`](peryx_driver::ServingState).
///
/// [`AppState`](peryx_driver::ServingState) is ecosystem-neutral and starts with no-op serving/indexing
/// defaults; the composition root (the binary, and the serving tests) calls this once so requests
/// dispatch through [`PypiServing`] and search indexes through [`PypiIndexer`].
#[cfg(feature = "serving")]
pub fn install(state: &mut peryx_driver::AppState) {
    state.register_ecosystem(std::sync::Arc::new(PypiServing), std::sync::Arc::new(PypiIndexer));
    // peryx's neutral vocabulary is Python's own (index, project, version, file), so the PyPI
    // lexicon is the neutral one; a future divergence would give this crate its own constant.
    state.register_lexicon(peryx_core::Ecosystem::Pypi, &peryx_core::Lexicon::NEUTRAL);
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
