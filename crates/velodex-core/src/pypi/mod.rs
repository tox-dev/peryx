//! The `PyPI` ecosystem: project names, versions, and the simple repository API.
//!
//! This is the only ecosystem velodex implements. It sits under its own module so a future ecosystem
//! can be added as a sibling rather than tangled into shared code.

mod html;
mod metadata;
mod name;
mod simple;
mod version;

pub use html::parse_detail_html;
pub use metadata::{CoreMetadataDoc, parse_metadata};
pub use name::{PackageName, file_matches_version, normalize_name};
pub use simple::{
    API_VERSION, CoreMetadata, File, Meta, ParsedDetail, ProjectDetail, ProjectList, ProjectListEntry, Yanked,
    parse_detail, render_detail_html, render_index_html, to_json,
};
pub use version::{Version, parse_version, sorted_desc};

#[cfg(test)]
mod tests;
