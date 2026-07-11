//! Building a [`PageContext`] from the virtual-index pieces and decoding overrides.

use std::collections::{HashMap, HashSet};

use peryx_policy::Policy;

use super::PageContext;
use crate::{File, Yanked};

/// Build a [`PageContext`] from the virtual-index pieces: hosted files shadow upstream filenames, hidden
/// overrides drop files, yank overrides mark them.
#[must_use]
pub fn page_context<S: std::hash::BuildHasher>(
    route: &str,
    project: &str,
    policy: Policy,
    local_files: Vec<File>,
    local_versions: Vec<String>,
    overrides: &HashMap<String, String, S>,
) -> PageContext {
    let mut skip: HashSet<String> = local_files.iter().map(|file| file.filename.clone()).collect();
    let mut yanked = HashMap::new();
    for (filename, kind) in overrides {
        match kind.as_str() {
            "hidden" => {
                skip.insert(filename.clone());
            }
            _ if let Some(marker) = yanked_override(kind) => {
                yanked.insert(filename.clone(), marker);
            }
            _ => {}
        }
    }
    PageContext {
        route: route.to_owned(),
        base: None,
        project: project.to_owned(),
        policy,
        local_files,
        local_versions,
        skip,
        yanked,
        known_metadata: HashMap::new(),
    }
}

pub fn hidden_override(value: &str) -> bool {
    value == "hidden"
}

pub fn yanked_override(value: &str) -> Option<Yanked> {
    if value == "yanked" {
        return Some(Yanked::Yes);
    }
    let record = serde_json::from_str::<StoredYankOverride>(value).ok()?;
    (record.kind == "yanked").then_some(match record.reason {
        Some(reason) if !reason.is_empty() => Yanked::Reason(reason),
        _ => Yanked::Yes,
    })
}

#[derive(serde::Deserialize)]
struct StoredYankOverride {
    kind: String,
    reason: Option<String>,
}
