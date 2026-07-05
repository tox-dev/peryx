//! The `PyPI` ecosystem driver for velodex: project names, versions, and the simple repository API.
//!
//! This crate implements the [`EcosystemDriver`] seam from `velodex-format` for Python. A future
//! ecosystem is a sibling `velodex-ecosystem-*` crate, so nothing here is tangled into shared code.

use velodex_format::{Ecosystem, EcosystemDriver};

#[cfg(feature = "serving")]
pub mod archive;
#[cfg(feature = "serving")]
pub mod cache;
#[cfg(feature = "serving")]
pub mod discovery;
mod filename;
mod html;
mod legacy_json;
mod metadata;
mod name;
#[cfg(feature = "serving")]
pub mod policy;
#[cfg(feature = "serving")]
pub mod search_pypi;
#[cfg(feature = "serving")]
pub mod serving;
mod simple;
#[cfg(feature = "serving")]
pub mod stream;
#[cfg(feature = "serving")]
pub mod upload;
mod version;

#[cfg(feature = "serving")]
pub use search_pypi::PypiIndexer;
#[cfg(feature = "serving")]
pub use serving::PypiServing;

pub use filename::{DistributionFilename, DistributionFilenameError, DistributionKind, parse_distribution_filename};
pub use html::{parse_detail_html, parse_index_html};
pub use legacy_json::render_legacy_json;
pub use metadata::{CoreMetadataDoc, parse_metadata};
pub use name::{PackageName, file_matches_version, is_valid_name, normalize_name, project_of_filename};
pub use simple::{
    API_VERSION, CoreMetadata, File, Meta, ParsedDetail, ProjectDetail, ProjectList, ProjectListEntry, ProjectStatus,
    Provenance, SimpleError, Yanked, parse_detail, parse_index, parse_meta, render_detail_html, render_index_html,
    to_json,
};
pub use version::{Version, VersionSpecifiers, parse_version, parse_version_specifiers, sorted_desc};

/// Wire the `PyPI` serving driver and search indexer into a freshly built
/// [`AppState`](velodex_http::AppState).
///
/// [`AppState`](velodex_http::AppState) is ecosystem-neutral and starts with no-op serving/indexing
/// defaults; the composition root (the binary, and the serving tests) calls this once so requests
/// dispatch through [`PypiServing`] and search indexes through [`PypiIndexer`].
#[cfg(feature = "serving")]
pub fn install(state: &mut velodex_http::AppState) {
    state.set_ecosystem(std::sync::Arc::new(PypiServing), std::sync::Arc::new(PypiIndexer));
}

/// The [`EcosystemDriver`] for the Python Package Index.
#[derive(Debug, Clone, Copy, Default)]
pub struct PypiDriver;

impl EcosystemDriver for PypiDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Pypi
    }
}

#[cfg(test)]
mod tests;
