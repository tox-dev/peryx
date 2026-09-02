//! What a scan that walks past a damaged record could not read.

use super::MetaError;

/// One stored row a repair scan could not decode.
#[derive(Debug)]
pub struct CorruptRecord {
    /// The row's key relative to its namespace prefix, which is the identifier a repair acts on.
    pub key: String,
    /// Why the row did not decode. Its message names the record, never the bytes it holds.
    pub source: MetaError,
}

/// The outcome of a scan that enumerates damage instead of stopping at the first damaged row.
///
/// A repair pass exists to see a whole namespace, so it has to outlive one bad record. That makes
/// the opposite mistake the dangerous one: reporting a namespace clean over rows it never decoded.
/// Carrying the skipped keys in the value a caller must already handle keeps that from happening
/// silently.
#[derive(Debug, Default)]
pub struct RepairScan {
    corrupt: Vec<CorruptRecord>,
}

impl RepairScan {
    /// Record that `key` could not be decoded, so the scan continues without claiming to have read it.
    pub fn skip(&mut self, key: impl Into<String>, source: MetaError) {
        self.corrupt.push(CorruptRecord {
            key: key.into(),
            source,
        });
    }

    /// Every row the scan could not decode, in the order it met them.
    #[must_use]
    pub fn corrupt(&self) -> &[CorruptRecord] {
        &self.corrupt
    }

    /// Whether the scan skipped a row, so its visitor saw less than the namespace holds.
    #[must_use]
    pub const fn is_incomplete(&self) -> bool {
        !self.corrupt.is_empty()
    }
}
