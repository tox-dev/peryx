use peryx_core::{RenderedDescription, UiAction, UiArtifactSource, UiByteAvailability};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProjectView {
    pub name: String,
    pub status: Option<Box<ProjectStatusView>>,
    pub versions: Vec<ReleaseView>,
    pub files: Vec<FileView>,
    pub actions: Vec<UiAction>,
    pub client_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStatusView {
    pub marker: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseView {
    pub version: String,
    pub lifecycle: Option<LifecycleView>,
    pub actions: Vec<UiAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleView {
    pub label: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileView {
    pub filename: String,
    pub release: Option<String>,
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
    pub lifecycle: Option<LifecycleView>,
    pub has_metadata: bool,
    pub browsable: bool,
    pub provenance: Option<String>,
    pub provenance_detail: Option<ProvenanceView>,
    pub upstream: Option<String>,
    pub source: UiArtifactSource,
    pub availability: UiByteAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MetadataView {
    pub version: Option<String>,
    pub summary: Option<String>,
    pub description: Option<RenderedDescription>,
    pub blocks: Vec<MetadataBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum MetadataBlock {
    KeyValue {
        label: String,
        value: String,
    },
    Chips {
        label: String,
        values: Vec<String>,
    },
    Links {
        label: String,
        links: Vec<(String, String)>,
    },
    Groups {
        label: String,
        groups: Vec<(String, Vec<String>)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceView {
    pub source: ProvenanceSource,
    pub attestations: Vec<AttestationView>,
    pub malformed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceSource {
    Hosted,
    Mirrored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationView {
    pub predicate_type: Option<String>,
    pub subject: SubjectMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectMatch {
    Matched,
    Mismatched,
    Unknown,
}
