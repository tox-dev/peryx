//! PEP 440 version parsing and ordering, via `pep440_rs`.

use std::str::FromStr as _;

pub use pep440_rs::Version;

/// Parse a PEP 440 version, returning `None` when the string is not a valid version.
#[must_use]
pub fn parse_version(text: &str) -> Option<Version> {
    Version::from_str(text).ok()
}

/// Sort version strings newest-first. Strings that do not parse as PEP 440 keep their input order
/// after the parseable ones.
#[must_use]
pub fn sorted_desc(versions: &[String]) -> Vec<String> {
    let mut parsed: Vec<(Option<Version>, &String)> = versions.iter().map(|v| (parse_version(v), v)).collect();
    parsed.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(x), Some(y)) => y.cmp(x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    parsed.into_iter().map(|(_, v)| v.clone()).collect()
}
