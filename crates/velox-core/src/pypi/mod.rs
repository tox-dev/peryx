//! The `PyPI` ecosystem: project names, versions, and the simple repository API.
//!
//! This is the only ecosystem velox implements. It sits under its own module so a future ecosystem
//! can be added as a sibling rather than tangled into shared code.

mod name;
mod simple;
mod version;

pub use name::{PackageName, normalize_name};
pub use simple::{
    API_VERSION, CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Yanked, render_detail_html,
    render_index_html, to_json,
};
pub use version::{Version, parse_version, sorted_desc};

#[cfg(test)]
mod tests;
