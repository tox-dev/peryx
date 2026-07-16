//! peryx library: the testable core of the binary (CLI, config, logging helpers, dispatch).
//!
//! `main.rs` is a thin shell over this crate that reads the real environment and installs the
//! global tracing subscriber; coverage excludes it.

pub mod api;
pub mod app;
pub mod cli;
pub mod config;
pub mod logging;
pub mod operator;
pub mod prefetch;
pub mod replication;
pub mod server;

#[cfg(test)]
mod tests;
