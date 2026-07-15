//! Dependency specifiers, the value grammar shared by the `Requires-Dist`, `Provides-Dist`, and
//! `Obsoletes-Dist` core-metadata fields.
//!
//! Core Metadata gives all three fields the PEP 508 requirement grammar — a project name, optional
//! extras, an optional version specifier, and an optional environment marker — so peryx defers to
//! the `pep508_rs` parser `packaging.requirements.Requirement` mirrors rather than reimplementing
//! the grammar.

use pep508_rs::{Requirement, VerbatimUrl};

/// Validate a PEP 508 dependency specifier, returning the reason it was rejected.
pub fn validate(value: &str) -> Result<(), &'static str> {
    value
        .parse::<Requirement<VerbatimUrl>>()
        .map_err(|_| "is not a valid dependency specifier")?;
    Ok(())
}
