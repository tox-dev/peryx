//! Per-ecosystem benchmark suites, one module each, mirroring the `velodex-ecosystem-*` crates.
//!
//! The neutral harness core they build on lives outside this module: `report`, `usage`, and the
//! server lifecycle in `servers`. Adding an ecosystem is a new module here plus an enum variant.

pub mod oci;
pub mod pypi;

/// The package ecosystem a suite targets; the first selection axis (`--ecosystem`).
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Ecosystem {
    /// The Python Package Index.
    Pypi,
    /// The OCI/Docker container registry.
    Oci,
}
