pub use peryx_core::{UiArtifactRef, UiFile, UiManifest, UiMember, UiMemberChunk, UiProject, UiProjectView};

use super::string_at;

/// Rebuild an archive listing from the inspect endpoint's JSON document.
#[must_use]
pub fn members_from_listing(value: &serde_json::Value) -> Vec<UiMember> {
    value["members"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|member| UiMember {
            path: string_at(member, "path"),
            size: member["size"].as_u64().unwrap_or_default(),
            kind: member["kind"].as_str().unwrap_or("unknown").to_owned(),
            previewable: member["previewable"].as_bool().unwrap_or(false),
        })
        .collect()
}

/// The project names of one index, from its PEP 691 list document.
#[must_use]
pub fn projects_from_list(value: &serde_json::Value) -> Vec<String> {
    value["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|project| string_at(project, "name"))
        .collect()
}
