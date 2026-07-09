//! The upstream index client: fetch and conditionally revalidate simple pages and files from a
//! PEP 503/691 index (pypi.org by default, or any configured upstream).

pub mod client;

pub use client::{
    Auth, FileHead, RangeError, SimpleHead, SimpleResponse, UpstreamClient, UpstreamError, UpstreamProtocol, redact_url,
};

#[cfg(test)]
mod tests;
