use std::error::Error;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

/// The wire format version for replication pages.
pub const PROTOCOL_VERSION: u16 = 1;

/// A content blob required by a journal change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobReference {
    pub sha256: String,
    pub size: u64,
}

/// One opaque metadata row mutation.
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

/// A serial change with enough data for a replica to reproduce it without interpreting an
/// ecosystem's metadata schema.
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

/// The authenticated primary boundary. HTTP transport and credentials live outside the replay
/// engine; implementations expose decoded pages and streamed blob bytes.
#[async_trait]
pub trait Primary: Sync {
    type Error: Error + Send + Sync + 'static;
    type BlobStream: Stream<Item = Result<Bytes, Self::Error>> + Send + Unpin;

    async fn changes(&self, after: u64, limit: usize) -> Result<ChangePage, Self::Error>;

    async fn blob(&self, digest: &peryx_storage::blob::Digest) -> Result<Self::BlobStream, Self::Error>;
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
