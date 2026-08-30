//! The reversible per-file override an operator records over a file served from a read-only layer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Yanked;

/// An administrative hide and a PEP 592 yank are independent facts about the same upstream file.
///
/// Each gets its own field in one record. A delete moves only `hidden` and a restore moves it back,
/// which leaves a yank an installer already honoured in place across the round trip; a yank moves
/// only `yanked`, which leaves a hidden file hidden.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileOverride {
    /// Whether an administrative delete withdrew the file from every served page.
    pub hidden: bool,
    /// The yank state the operator imposed, where [`Yanked::No`] means the file carries no yank
    /// override and the upstream file's own yank state is served unchanged.
    pub yanked: Yanked,
}

impl FileOverride {
    /// Decode a stored override, or `None` when the record does not parse.
    #[must_use]
    pub fn decode(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }

    /// # Panics
    /// Never: a bool and a `Yanked` always serialize.
    #[must_use]
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("file override always serializes")
    }

    /// Whether the record imposes nothing, in which case the key is removed rather than stored.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.hidden && matches!(self.yanked, Yanked::No)
    }

    /// Decode a project's stored overrides, dropping any record that does not parse so a corrupt
    /// row - which `fsck` reports - cannot hide or yank a file by accident.
    #[must_use]
    pub fn decode_all(entries: Vec<(String, String)>) -> BTreeMap<String, Self> {
        entries
            .into_iter()
            .filter_map(|(filename, raw)| Some((filename, Self::decode(&raw)?)))
            .collect()
    }
}

/// The single-field change a delete, restore, yank, or unyank applies to a file's override record.
#[derive(Clone, Copy)]
pub enum OverrideMutation<'a> {
    Hidden(bool),
    Yanked(&'a Yanked),
}

impl OverrideMutation<'_> {
    /// Apply the change to `record`, returning the journal action a replica replays when the record
    /// actually moved and `None` when it already held the requested value.
    pub(super) fn apply(self, record: &mut FileOverride) -> Option<&'static str> {
        match self {
            Self::Hidden(hidden) => {
                if record.hidden == hidden {
                    return None;
                }
                record.hidden = hidden;
                Some(if hidden { "hide" } else { "restore" })
            }
            Self::Yanked(yanked) => {
                if record.yanked == *yanked {
                    return None;
                }
                record.yanked = yanked.clone();
                Some(if matches!(yanked, Yanked::No) { "unyank" } else { "yank" })
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/store/overrides/tests.rs"]
mod tests;
