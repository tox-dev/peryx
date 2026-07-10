//! HTTP-level serving tests for the `PyPI` driver, one module per concept.

mod support;

mod discovery;
mod download;
mod inspect;
mod legacy_json;
mod metadata;
mod mirror;
mod mutate;
mod overlay;
mod policy;
mod promote;
mod render_cache;
mod routing;
mod security;
mod status;
mod upload;

pub use support::*;
