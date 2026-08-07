//! Trove classifiers, the value grammar of the `Classifier` core-metadata field.
//!
//! Core Metadata defers to the list `PyPI` publishes, and that list grows - a new Python release
//! adds a classifier that yesterday's peryx would have rejected. Validation has to answer offline,
//! so the list is vendored by `ci/vendor-classifiers.py` rather than fetched, and staying current
//! is a generator run.

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
