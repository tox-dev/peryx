//! Turning the registry's stored manifests and layers into the neutral web view models, so the web
//! crate renders an OCI repository, manifest, and layer without parsing any `/v2/` wire document.

use peryx_core::{UiArtifactRef, UiManifest, UiMember};

/// Parse a stored manifest's JSON bytes into the neutral manifest view.
///
/// # Errors
/// Returns a message when the bytes are not valid JSON.
pub fn manifest_from_bytes(bytes: &[u8]) -> Result<UiManifest, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    Ok(manifest_from_json(&value))
}

/// Shape a manifest document into the neutral view: an image index of per-platform children, or an
/// image manifest with a config blob and layers. The total size sums what the view shows.
fn manifest_from_json(value: &serde_json::Value) -> UiManifest {
    let media_type = string_at(value, "mediaType");
    if let Some(children) = value["manifests"].as_array() {
        let entries: Vec<UiArtifactRef> = children.iter().map(artifact_ref).collect();
        let total_size = entries.iter().map(|entry| entry.size).sum();
        return UiManifest {
            media_type,
            is_index: true,
            config: None,
            entries,
            total_size,
        };
    }
    let config = value["config"].is_object().then(|| artifact_ref(&value["config"]));
    let entries: Vec<UiArtifactRef> = value["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(artifact_ref)
        .collect();
    let total_size = config.as_ref().map_or(0, |blob| blob.size) + entries.iter().map(|entry| entry.size).sum::<u64>();
    UiManifest {
        media_type,
        is_index: false,
        config,
        entries,
        total_size,
    }
}

/// One referenced blob or child manifest as a neutral view item. `browsable` is decided here — a tar
/// layer the archive engine can list — so shared web code never inspects a media type.
fn artifact_ref(value: &serde_json::Value) -> UiArtifactRef {
    let platform = value["platform"].is_object().then(|| {
        format!(
            "{}/{}",
            string_at(&value["platform"], "os"),
            string_at(&value["platform"], "architecture")
        )
    });
    let media_type = string_at(value, "mediaType");
    let browsable = media_type.contains("tar");
    UiArtifactRef {
        digest: string_at(value, "digest"),
        size: value["size"].as_u64().unwrap_or(0),
        media_type,
        platform,
        browsable,
    }
}

/// Rebuild a layer's member listing from the neutral archive-inspect document the layer browser serves.
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

/// Parse a `u64` response header, or `None` when it is absent or unparsable.
pub fn header_u64(headers: &axum::http::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}
