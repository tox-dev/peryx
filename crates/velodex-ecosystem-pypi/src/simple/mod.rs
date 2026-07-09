//! The PEP 503 / 691 simple repository API: data model and byte-exact serialization.
//!
//! velodex precomputes these responses at index-update time and serves the bytes, so both the JSON
//! (PEP 691) and HTML (PEP 503) forms are produced here once from the same model.

mod error;
mod file;
mod meta;
mod parse;
mod render;

pub use error::SimpleError;
pub use file::{CoreMetadata, File, Provenance, Yanked};
pub use meta::{API_VERSION, Meta, ProjectStatus};
pub use parse::{
    ParsedDetail, ProjectDetail, ProjectList, ProjectListEntry, parse_detail, parse_index, parse_meta, to_json,
};
pub use render::{render_detail_html, render_index_html};
