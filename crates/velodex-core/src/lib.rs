//! Core domain logic for velodex.
//!
//! This crate is pure: no I/O, no async runtime, no storage dependency, so its logic is fast and
//! deterministic to test.
//!
//! Everything ecosystem-specific lives under a named module. velodex implements only the Python
//! ([`pypi`]) ecosystem today; the module boundary leaves room to add others (npm, crates, …)
//! beside it later without reworking the crates that depend on this one.

pub mod pypi;
pub mod url_encoding;
