//! PEP 503 project-name normalization.

use std::borrow::Cow;
use std::fmt;

/// Normalize a project name per PEP 503: lowercase, and collapse every run of `-`, `_`, or `.`
/// into a single `-`. Equivalent to Python's `re.sub(r"[-_.]+", "-", name).lower()`.
#[must_use]
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_separator = false;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !in_separator {
                out.push('-');
                in_separator = true;
            }
        } else {
            in_separator = false;
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// [`normalize_name`] without the allocation when the name is already normalized, which upstream
/// indexes usually deliver. The check is a byte scan; only a name that needs rewriting is copied.
#[must_use]
pub fn normalize_name_cow(name: &str) -> Cow<'_, str> {
    if is_normalized(name) {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(normalize_name(name))
    }
}

/// Whether `name` is already in PEP 503 normal form: ASCII lowercase, digits, and single `-`
/// separators. A `true` here guarantees [`normalize_name`] would return `name` unchanged.
fn is_normalized(name: &str) -> bool {
    let mut prev_dash = false;
    for &byte in name.as_bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => prev_dash = false,
            b'-' if !prev_dash => prev_dash = true,
            _ => return false,
        }
    }
    true
}

/// The project a distribution filename belongs to: the escaped name before the first `-`, normalized
/// per PEP 503. Used to key usage aggregation by project when only the filename is at hand.
#[must_use]
pub fn project_of_filename(filename: &str) -> String {
    normalize_name(filename.split('-').next().unwrap_or(filename))
}

/// Whether `name` matches the `PyPA` project-name grammar before normalization.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// A project name in its normalized (PEP 503) form. Two spellings that normalize equal compare
/// equal, so this is the correct key for lookups and storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageName(String);

impl PackageName {
    /// Normalize `raw` and wrap it.
    #[must_use]
    pub fn new(raw: &str) -> Self {
        Self(normalize_name(raw))
    }

    /// The normalized name as a string slice.
    #[must_use]
    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether a distribution filename belongs to `version` of a project.
///
/// The version is read with [`distribution_version_segment`], which places it after the first `-` for
/// a wheel but the last `-` for an sdist whose project name may itself contain `-`. Versions compare
/// PEP 440-equal, so `1.0` matches a file of `1.0.0` but never one of `1.0.1`.
#[must_use]
pub fn file_matches_version(filename: &str, version: &str) -> bool {
    let Some(candidate) = super::distribution_version_segment(filename) else {
        return false;
    };
    candidate == version
        || matches!(
            (super::parse_version(candidate), super::parse_version(version)),
            (Some(file_version), Some(wanted)) if file_version == wanted
        )
}
