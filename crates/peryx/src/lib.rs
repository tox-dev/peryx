//! peryx library: the testable core of the binary (CLI, config, logging helpers, dispatch).
//!
//! `main.rs` is a thin shell over this crate that reads the real environment and installs the
//! global tracing subscriber; coverage excludes it.

use peryx_ecosystem_oci as _;
use peryx_ecosystem_pypi as _;

pub mod api;
pub mod app;
pub mod availability;
pub mod cli;
pub mod config;
pub mod logging;
pub mod operator;
pub mod prefetch;
pub mod replication;
pub mod server;

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
