//! The document shape a pushed manifest must have before peryx stores it. A hosted push declares a
//! media type, the media type selects one of the two OCI image-spec schemas, and the body is checked
//! against it: a proxy stores whatever an upstream sends verbatim, but bytes an authoritative push
//! accepts are bytes peryx promises to serve back as a manifest, so they have to parse as one.

use serde_json::Value;

/// The manifest document shape a supported media type declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestSchema {
    /// An image manifest: one config descriptor and a layer list.
    Image,
    /// An image index or Docker manifest list: a list of child manifest descriptors.
    Index,
}

/// Why a pushed manifest body does not satisfy the schema its media type declares.
#[derive(Debug, thiserror::Error)]
pub enum ManifestSchemaError {
    #[error("manifest body is not JSON: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("manifest body is not a JSON object")]
    NotAnObject,
    #[error("manifest schemaVersion must be 2")]
    SchemaVersion,
    #[error("manifest mediaType must be {0}")]
    MediaType(String),
    #[error("manifest is missing the required {0} field")]
    Missing(&'static str),
    #[error("manifest {0} must be an array of descriptors")]
    NotAList(&'static str),
    #[error("the {location} descriptor requires {requirement}")]
    Descriptor {
        location: String,
        requirement: &'static str,
    },
}

impl ManifestSchema {
    /// The schema a hosted push may store under this media type: the OCI image manifest and index and
    /// the Docker v2 schema-2 manifest and manifest list. `None` is a media type a hosted push rejects
    /// rather than serving it back to a puller as a manifest.
    #[must_use]
    pub fn of(media_type: &str) -> Option<Self> {
        match media_type {
            "application/vnd.oci.image.manifest.v1+json" | "application/vnd.docker.distribution.manifest.v2+json" => {
                Some(Self::Image)
            }
            "application/vnd.oci.image.index.v1+json" | "application/vnd.docker.distribution.manifest.list.v2+json" => {
                Some(Self::Index)
            }
            _ => None,
        }
    }

    /// Check a pushed body against this schema and hand back the document it parsed, so the write path
    /// reads the descriptors it already validated instead of parsing the bytes again. Fields the spec
    /// does not define are extension data and pass untouched, so an artifact manifest, a foreign
    /// layer's `urls` and an index entry's `platform` are all accepted.
    ///
    /// # Errors
    /// Returns the first rule the document breaks.
    pub fn validate(self, declared: &str, bytes: &[u8]) -> Result<Value, ManifestSchemaError> {
        let document: Value = serde_json::from_slice(bytes)?;
        let Some(fields) = document.as_object() else {
            return Err(ManifestSchemaError::NotAnObject);
        };
        if fields.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
            return Err(ManifestSchemaError::SchemaVersion);
        }
        // The declared type is what peryx records and hands back as `Content-Type`, so a body claiming
        // another one would be served under a media type it contradicts.
        if fields.get("mediaType").and_then(Value::as_str) != Some(declared) {
            return Err(ManifestSchemaError::MediaType(declared.to_owned()));
        }
        match self {
            Self::Image => {
                descriptor(required(fields, "config")?, || "config".to_owned())?;
                descriptor_list(fields, "layers")?;
            }
            Self::Index => descriptor_list(fields, "manifests")?,
        }
        if let Some(subject) = fields.get("subject") {
            descriptor(subject, || "subject".to_owned())?;
        }
        Ok(document)
    }
}

fn required<'a>(
    document: &'a serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<&'a Value, ManifestSchemaError> {
    document.get(field).ok_or(ManifestSchemaError::Missing(field))
}

fn descriptor_list(document: &serde_json::Map<String, Value>, field: &'static str) -> Result<(), ManifestSchemaError> {
    let entries = required(document, field)?
        .as_array()
        .ok_or(ManifestSchemaError::NotAList(field))?;
    for (position, entry) in entries.iter().enumerate() {
        descriptor(entry, || format!("{field}[{position}]"))?;
    }
    Ok(())
}

/// Check one descriptor, naming where it sits in the document only when it is the one at fault, so a
/// long layer list costs no formatting on the accepting path.
fn descriptor(value: &Value, location: impl FnOnce() -> String) -> Result<(), ManifestSchemaError> {
    let requirement = if !value.is_object() {
        "a JSON object"
    } else if value["mediaType"].as_str().is_none_or(str::is_empty) {
        "a mediaType string"
    } else if value["digest"].as_str().is_none_or(str::is_empty) {
        "a digest string"
    } else if value["size"].as_u64().is_none() {
        "a non-negative integer size"
    } else {
        return Ok(());
    };
    Err(ManifestSchemaError::Descriptor {
        location: location(),
        requirement,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/store/schema/tests.rs"]
mod tests;
