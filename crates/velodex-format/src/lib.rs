//! Ecosystem-neutral domain core for velodex.
//!
//! This crate is pure: no I/O, no async runtime, no storage dependency, so its logic is fast and
//! deterministic to test.
//!
//! It owns the [`Ecosystem`] axis and the neutral [`Lexicon`]. velodex implements the Python ecosystem
//! today (the `velodex-ecosystem-pypi` crate); further ecosystems (npm, crates, OCI, …) are sibling
//! crates that add an [`Ecosystem`] variant and register a serving driver, without reworking the crates
//! that depend on this one.

pub mod ecosystem;
pub mod url_encoding;

pub use ecosystem::{Ecosystem, Lexicon, UnknownEcosystem};
