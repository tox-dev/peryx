//! Content-addressed blob storage with atomic publication at `sha256/ab/cd/<digest>`.

use std::path::Path;

mod backend;
mod chunked;
mod durability;
mod error;
mod range;
mod s3;
mod stage;
mod storage;
mod store;

pub use backend::{
    BlobBackend, BlobCapabilities, BlobLease, BlobRead, BlobReadBody, BlobStaged, BlobSupport, BlobTail, BlobWrite,
};
pub use chunked::{CHUNK_BYTES, ChunkedDigest, ChunkedDigestBuilder};
pub use durability::{DurabilityCapabilities, DurabilityShortfall, PlacementReceipt, Publication};
pub use error::{BlobError, BlobErrorContext, BlobErrorKind, BlobOperation, BlobScanError};
pub use peryx_core::{BlobDurability, BlobMetadata, Digest, DurabilityRequirement, WriteEvidence};
pub use range::{RangeRequest, parse_range};
pub use s3::{S3Addressing, S3Backend, S3Client, S3Config, S3ConfigError, S3Error, S3Settings};
pub use stage::StageUsage;
pub use storage::{BlobBlocking, BlobStorage};
pub use store::{BlobEntry, BlobStore, PendingBlob, StagedBlob};

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Syncing the file does not make its rename crash-durable. Directory sync failures do not fail the write.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(directory) = std::fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/blob/sweep_tests.rs"]
mod sweep_tests;
