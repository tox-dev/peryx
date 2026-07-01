//! PEP 503 project-name normalization.

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
