//! Streaming transformation of upstream PEP 691 pages.
//!
//! The transformer consumes raw upstream JSON chunk by chunk (mid-token boundaries included) and
//! emits the page peryx serves, without ever holding more than one `files[]` element: file URLs are
//! rewritten to the serving route, locally uploaded files are injected ahead of the upstream ones,
//! shadowed and hidden files are dropped, yank overrides are applied, and version lists merge. The
//! client starts receiving bytes while the upstream download is still in flight, so a cold page
//! costs wire time, not wire time plus parse-transform-serialize.

mod context;
mod transformer;
mod types;
mod validator;

pub use context::page_context;
pub use transformer::PageTransformer;
pub(crate) use transformer::{MAX_PAGE_BYTES, metadata_sibling};
pub use types::{PageContext, PageSummary, Registration, TransformError};

/// RFC 8259 allows space, horizontal tab, line feed, and carriage return between JSON tokens.
const fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}
