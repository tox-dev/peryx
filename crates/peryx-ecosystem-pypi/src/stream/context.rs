use std::collections::{BTreeMap, BTreeSet};

use peryx_policy::Policy;

use super::PageContext;
use crate::store::FileOverride;
use crate::{File, Yanked};

#[must_use]
pub fn page_context(
    route: &str,
    project: &str,
    policy: Policy,
    local_files: Vec<File>,
    local_versions: Vec<String>,
    overrides: &BTreeMap<String, FileOverride>,
) -> PageContext {
    let mut skip: BTreeSet<String> = local_files.iter().map(|file| file.filename.clone()).collect();
    let mut hidden = BTreeSet::new();
    let mut yanked = BTreeMap::new();
    for (filename, record) in overrides {
        if record.hidden {
            skip.insert(filename.clone());
            hidden.insert(filename.clone());
        }
        if record.yanked != Yanked::No {
            yanked.insert(filename.clone(), record.yanked.clone());
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
        hidden,
        yanked,
        known_metadata: BTreeMap::new(),
    }
}
