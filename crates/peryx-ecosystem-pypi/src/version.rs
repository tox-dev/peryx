//! PEP 440 version parsing and ordering, via `pep440_rs`.

use std::str::FromStr as _;

pub use pep440_rs::{Version, VersionSpecifiers};

/// Parse a PEP 440 version, returning `None` when the string is not a valid version.
#[must_use]
pub fn parse_version(text: &str) -> Option<Version> {
    Version::from_str(text).ok()
}

/// Parse PEP 440 version specifiers, returning `None` when the string is invalid.
#[must_use]
pub fn parse_version_specifiers(text: &str) -> Option<VersionSpecifiers> {
    VersionSpecifiers::from_str(text).ok()
}

/// Whether two strings name the same release under PEP 440, so `1.0` and `1.0.0` match.
///
/// Falls back to byte equality when either side is not a valid PEP 440 version, so an unparseable
/// spelling still matches itself.
#[must_use]
pub fn versions_match(left: &str, right: &str) -> bool {
    left == right || matches!((parse_version(left), parse_version(right)), (Some(left), Some(right)) if left == right)
}

/// A version identity that matches [`versions_match`]: two versions are the same release when their
/// strings are equal or they parse to the same PEP 440 version. Grouping by this key collapses a
/// per-version rescan of the files into one pass.
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VersionKey {
    Parsed(Version),
    Raw(String),
}

pub fn version_key(version: &str) -> VersionKey {
    parse_version(version).map_or_else(|| VersionKey::Raw(version.to_owned()), VersionKey::Parsed)
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
