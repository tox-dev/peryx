//! The upstream index client: fetch and conditionally revalidate simple pages and files from a
//! PEP 503/691 index (pypi.org by default, or any configured mirror).

pub mod client;

pub use client::{Auth, SimpleHead, SimpleResponse, UpstreamClient, UpstreamError};

#[cfg(test)]
mod tests;
