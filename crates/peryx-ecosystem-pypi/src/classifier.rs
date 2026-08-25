//! Core Metadata defers to the list `PyPI` publishes, and validation has to answer offline. The
//! canonical `pypa/trove-classifiers` data is therefore vendored in this module.

mod data;

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Validate a trove classifier, returning the reason it was rejected.
pub fn validate(value: &str) -> Result<(), &'static str> {
    static KNOWN: LazyLock<HashSet<&'static str>> = LazyLock::new(|| data::KNOWN.into_iter().collect());
    static DEPRECATED: LazyLock<HashMap<&'static str, &'static str>> =
        LazyLock::new(|| data::DEPRECATED.into_iter().collect());

    if let Some(reason) = DEPRECATED.get(value) {
        return Err(reason);
    }
    if KNOWN.contains(value) {
        Ok(())
    } else {
        Err("is not a known trove classifier")
    }
}
