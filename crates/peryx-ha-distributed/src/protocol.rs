use std::error::Error;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobReference {
    pub sha256: String,
    pub size: u64,
}

impl From<peryx_storage::meta::DriverBlobReference> for BlobReference {
    fn from(reference: peryx_storage::meta::DriverBlobReference) -> Self {
        Self {
            sha256: reference.sha256,
            size: reference.size,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum MetadataMutation {
    Put {
        key: String,
        #[serde(with = "base64_bytes")]
        value: Vec<u8>,
    },
    Delete {
        key: String,
    },
}

impl MetadataMutation {
    pub(crate) fn key(&self) -> &str {
        match self {
            Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }
}

impl From<peryx_storage::meta::DriverMutation> for MetadataMutation {
    fn from(mutation: peryx_storage::meta::DriverMutation) -> Self {
        match mutation {
            peryx_storage::meta::DriverMutation::Put { key, value } => Self::Put { key, value },
            peryx_storage::meta::DriverMutation::Delete { key } => Self::Delete { key },
        }
    }
}

/// A serial change a replica can apply without interpreting the ecosystem's metadata schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub serial: u64,
    #[serde(with = "base64_bytes")]
    pub event: Vec<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<MetadataMutation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blobs: Vec<BlobReference>,
}

/// A page read from one stable primary identity after an exclusive serial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePage {
    pub version: u16,
    pub source: String,
    pub after: u64,
    pub current_serial: u64,
    pub changes: Vec<Change>,
}

/// Returns decoded changes from an authenticated primary; transport and credentials remain outside
/// the replay engine.
#[async_trait]
pub trait Primary: Sync {
    type Error: Error + Send + Sync + 'static;

    async fn changes(&self, after: u64, limit: usize) -> Result<ChangePage, Self::Error>;
}

/// Peer-visible blob placement state; storage transitions remain internal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementAvailability {
    Pending,
    Verified,
    Failed,
    Revoked,
}

impl From<peryx_ha::BlobPlacementStatus> for PlacementAvailability {
    fn from(status: peryx_ha::BlobPlacementStatus) -> Self {
        match status {
            peryx_ha::BlobPlacementStatus::Pending => Self::Pending,
            peryx_ha::BlobPlacementStatus::Verified => Self::Verified,
            peryx_ha::BlobPlacementStatus::Failed => Self::Failed,
            peryx_ha::BlobPlacementStatus::Revoked => Self::Revoked,
        }
    }
}

/// Peer-visible blob placement without storage fencing or timing fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementDescriptor {
    pub digest: String,
    pub backend: String,
    pub data_center: String,
    pub location: String,
    pub availability: PlacementAvailability,
    pub generation: u64,
}

impl From<&peryx_ha::BlobPlacementRecord> for PlacementDescriptor {
    fn from(record: &peryx_ha::BlobPlacementRecord) -> Self {
        Self {
            digest: record.key.digest.canonical(),
            backend: record.key.backend.as_str().to_owned(),
            data_center: record.key.data_center.as_str().to_owned(),
            location: record.key.location.as_str().to_owned(),
            availability: record.state.status().into(),
            generation: record.generation,
        }
    }
}

mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = <String>::deserialize(deserializer)?;
        STANDARD.decode(encoded).map_err(serde::de::Error::custom)
    }
}
