//! Ecosystem-neutral domain core for velodex.
//!
//! This crate is pure: no I/O, no async runtime, no storage dependency, so its logic is fast and
//! deterministic to test.
//!
//! It owns the [`Ecosystem`] axis and the [`EcosystemDriver`] seam every package format plugs into.
//! velodex implements only the Python ([`pypi`]) ecosystem today; its driver moves to the
//! `velodex-ecosystem-pypi` crate, and further ecosystems (npm, crates, OCI, …) are sibling crates
//! that add an [`Ecosystem`] variant and an [`EcosystemDriver`] impl without reworking the crates that
//! depend on this one.

pub mod ecosystem;
pub mod pypi;
pub mod url_encoding;

pub use ecosystem::{Ecosystem, EcosystemDriver, UnknownEcosystem};
