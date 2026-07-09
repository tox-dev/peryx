use serde::{Deserialize, Serialize};

/// A cached upstream simple-index response plus the metadata needed to revalidate it. The body is
/// the raw upstream document; velodex transforms it per request, so one cached page serves any route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedIndex {
    pub etag: Option<String>,
    pub last_serial: Option<u64>,
    pub fetched_at_unix: i64,
    #[serde(default)]
    pub content_type: Option<String>,
    /// The freshness lifetime upstream granted via `Cache-Control`; `None` means the server sent
    /// no usable lifetime and the configured fallback applies.
    #[serde(default)]
    pub fresh_secs: Option<i64>,
    pub body: Vec<u8>,
}

/// A cached simple-index record summary that does not copy the page body for framed records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedIndexSummary {
    pub fetched_at_unix: i64,
    pub fresh_secs: Option<i64>,
    pub body_bytes: u64,
    pub record_bytes: u64,
    pub last_serial: Option<u64>,
    pub content_type: Option<String>,
}

/// A cached simple-index record keyed by its metadata table key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedIndexPage {
    pub key: String,
    pub summary: CachedIndexSummary,
}

/// One project's explicit Simple API status marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStatusRecord {
    pub status: Option<String>,
    pub reason: Option<String>,
}

/// Marks the framed record encoding: a JSON header line, then the raw body bytes.
const RECORD_PREFIX: &[u8] = b"velodex1\n";

/// The revalidation metadata of a [`CachedIndex`], stored as one compact JSON line ahead of the
/// body. Serializing the body inside JSON would turn megabytes of page into an array of numbers,
/// quadrupling storage and dominating every warm read.
#[derive(Serialize, Deserialize)]
struct RecordHeader {
    etag: Option<String>,
    last_serial: Option<u64>,
    fetched_at_unix: i64,
    content_type: Option<String>,
    #[serde(default)]
    fresh_secs: Option<i64>,
}

impl CachedIndex {
    /// Encode to bytes for storage: prefix, header line, raw body.
    ///
    /// # Panics
    /// Never in practice: every header field is serializable.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let header = serde_json::to_vec(&RecordHeader {
            etag: self.etag.clone(),
            last_serial: self.last_serial,
            fetched_at_unix: self.fetched_at_unix,
            content_type: self.content_type.clone(),
            fresh_secs: self.fresh_secs,
        })
        .expect("record header always serializes");
        let mut out = Vec::with_capacity(RECORD_PREFIX.len() + header.len() + 1 + self.body.len());
        out.extend_from_slice(RECORD_PREFIX);
        out.extend_from_slice(&header);
        out.push(b'\n');
        out.extend_from_slice(&self.body);
        out
    }

    /// Decode from stored bytes, accepting both the framed encoding and the plain-JSON records
    /// written by earlier versions.
    ///
    /// # Errors
    /// Returns the serde error when `bytes` is not a valid encoding.
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let Some((header, body)) = Self::split_framed(bytes) else {
            return serde_json::from_slice(bytes);
        };
        let header: RecordHeader = serde_json::from_slice(header)?;
        Ok(Self {
            etag: header.etag,
            last_serial: header.last_serial,
            fetched_at_unix: header.fetched_at_unix,
            content_type: header.content_type,
            fresh_secs: header.fresh_secs,
            body: body.to_vec(),
        })
    }

    /// Decode only the revalidation metadata, skipping the body copy; the refresher scans every
    /// record and needs nothing else.
    ///
    /// # Errors
    /// Returns the serde error when `bytes` is not a valid encoding.
    pub(super) fn decode_freshness(bytes: &[u8]) -> Result<(i64, Option<i64>), serde_json::Error> {
        let summary = Self::summary(bytes)?;
        Ok((summary.fetched_at_unix, summary.fresh_secs))
    }

    /// Decode cache-inspection metadata, skipping the body copy for framed records.
    ///
    /// # Errors
    /// Returns the serde error when `bytes` is not a valid encoding.
    pub fn summary(bytes: &[u8]) -> Result<CachedIndexSummary, serde_json::Error> {
        if let Some((header, body)) = Self::split_framed(bytes) {
            let header: RecordHeader = serde_json::from_slice(header)?;
            return Ok(CachedIndexSummary {
                fetched_at_unix: header.fetched_at_unix,
                fresh_secs: header.fresh_secs,
                body_bytes: body.len() as u64,
                record_bytes: bytes.len() as u64,
                last_serial: header.last_serial,
                content_type: header.content_type,
            });
        }
        let record: Self = serde_json::from_slice(bytes)?;
        Ok(CachedIndexSummary {
            fetched_at_unix: record.fetched_at_unix,
            fresh_secs: record.fresh_secs,
            body_bytes: record.body.len() as u64,
            record_bytes: bytes.len() as u64,
            last_serial: record.last_serial,
            content_type: record.content_type,
        })
    }

    /// Split a framed record into its header line and body, or `None` for legacy records.
    fn split_framed(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
        let rest = bytes.strip_prefix(RECORD_PREFIX)?;
        let split = rest.iter().position(|&byte| byte == b'\n')?;
        Some((&rest[..split], &rest[split + 1..]))
    }
}
