use serde::{Deserialize, Serialize};

use super::string_at;

/// One referenced object in an OCI manifest: a config blob, a layer, or a per-platform child manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiOciBlob {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    /// `os/architecture` when this entry is a per-platform manifest in an image index.
    pub platform: Option<String>,
}

/// One tag's manifest, parsed for the browser: an image manifest (a config blob plus layers) or an
/// image index (a list of per-platform child manifests).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiOciManifest {
    pub media_type: String,
    pub is_index: bool,
    pub config: Option<UiOciBlob>,
    /// Layers of an image manifest, or the per-platform manifests of an image index.
    pub entries: Vec<UiOciBlob>,
    pub total_size: u64,
}

impl UiOciManifest {
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Self {
        let media_type = string_at(value, "mediaType");
        if let Some(children) = value["manifests"].as_array() {
            let entries: Vec<UiOciBlob> = children.iter().map(blob_from).collect();
            let total_size = entries.iter().map(|entry| entry.size).sum();
            return Self {
                media_type,
                is_index: true,
                config: None,
                entries,
                total_size,
            };
        }
        let config = value["config"].is_object().then(|| blob_from(&value["config"]));
        let entries: Vec<UiOciBlob> = value["layers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(blob_from)
            .collect();
        let total_size =
            config.as_ref().map_or(0, |blob| blob.size) + entries.iter().map(|entry| entry.size).sum::<u64>();
        Self {
            media_type,
            is_index: false,
            config,
            entries,
            total_size,
        }
    }
}

fn blob_from(value: &serde_json::Value) -> UiOciBlob {
    let platform = value["platform"].is_object().then(|| {
        format!(
            "{}/{}",
            string_at(&value["platform"], "os"),
            string_at(&value["platform"], "architecture")
        )
    });
    UiOciBlob {
        digest: string_at(value, "digest"),
        size: value["size"].as_u64().unwrap_or(0),
        media_type: string_at(value, "mediaType"),
        platform,
    }
}
