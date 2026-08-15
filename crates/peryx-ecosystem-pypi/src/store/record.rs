use serde::{Deserialize, Serialize};

/// A cached upstream simple-index response plus the metadata needed to revalidate it. The body is
/// the raw upstream document; peryx transforms it per request, so one cached page serves any route.
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

/// A cached simple-index record keyed by its driver-KV key.
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

/// A mutable provenance object advertised by an upstream Simple API file entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamAttestation {
    /// The normalized project whose current page or published generation advertises the URL.
    pub project: String,
    /// The secure URL advertised by the upstream Simple API record.
    pub url: String,
    /// The configured cached index whose client can fetch `url`.
    pub source: String,
    /// The routed upstream that advertised `url`, when the cached index has named sources.
    pub upstream: Option<String>,
    /// The normalized media type returned with the last structurally accepted body.
    pub media_type: Option<String>,
    /// The upstream validator used before `Last-Modified` when both are present.
    pub etag: Option<String>,
    /// The upstream validator used when no `ETag` is present.
    pub last_modified: Option<String>,
    /// The serving clock when the body was fetched or revalidated.
    pub fetched_at_unix: Option<i64>,
    /// The shared-cache freshness lifetime granted by the upstream.
    pub fresh_secs: Option<i64>,
    /// Whether a stale body requires successful validation before reuse.
    #[serde(default)]
    pub must_revalidate: bool,
    /// Whether the structurally accepted, unverified body is retained locally.
    pub availability: AttestationAvailability,
    /// The exact structurally accepted, unverified JSON text, retained inline only in cache mode.
    pub body: Option<String>,
}

/// Whether an upstream provenance object still lives only at its source or has a retained body.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttestationAvailability {
    #[default]
    RemoteOnly,
    Cached,
}

impl UpstreamAttestation {
    /// Create a remote-only record for an upstream Simple API provenance link.
    #[must_use]
    pub fn remote(url: &str, source: &str, project: &str, upstream: Option<&str>) -> Self {
        Self {
            project: project.to_owned(),
            url: url.to_owned(),
            source: source.to_owned(),
            upstream: upstream.map(str::to_owned),
            media_type: None,
            etag: None,
            last_modified: None,
            fetched_at_unix: None,
            fresh_secs: None,
            must_revalidate: false,
            availability: AttestationAvailability::RemoteOnly,
            body: None,
        }
    }
}

/// One completely parsed remote project-detail generation: its provenance, its revalidation
/// validators, and the counts that let a later sweep reason about it without reading the file rows.
///
/// The per-file metadata rows a generation owns live under a generation-scoped key prefix; this
/// record is the small pointer a reader loads to find the active generation and revalidate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGeneration {
    pub generation: u64,
    /// The configured cached-index name that produced this generation.
    pub source: String,
    /// The redacted final URL the detail page was fetched from.
    pub url: String,
    /// The response format the generation was parsed from: `"json"` or `"html"`.
    pub format: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub last_serial: Option<u64>,
    /// When the detail page was last observed (a `200` parse or a `304` revalidation).
    pub fetched_at_unix: i64,
    /// The response byte length the generation was parsed from.
    pub bytes: u64,
    /// The number of policy-admitted file rows the generation holds.
    pub files: u64,
    /// The PEP 700 `versions` list, retained so a reader need not scan the file rows to list them.
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub project_status: Option<String>,
    #[serde(default)]
    pub project_status_reason: Option<String>,
}

/// Publication state for one cached index project's remote file-metadata generations.
///
/// `active` is the generation a reader serves; `staging` is a reservation an in-progress sync holds;
/// `retired` is the generation a publication just displaced, kept until its rows are swept. The shape
/// mirrors the root catalog's [`CatalogState`](super::CatalogState) so both durable syncs recover the
/// same way after an interrupted run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMetaState {
    pub active: Option<ProjectGeneration>,
    pub staging: Option<u64>,
    pub retired: Option<u64>,
    pub next_generation: u64,
}

/// The freshness fields a `304 Not Modified` advances: the fetch time and the granted lifetime.
///
/// A revalidation leaves the page body untouched, so these live in their own small row that a `304`
/// rewrites on its own - the record's multi-megabyte body row stays put.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessOverlay {
    pub fetched_at_unix: i64,
    #[serde(default)]
    pub fresh_secs: Option<i64>,
}

impl FreshnessOverlay {
    /// # Panics
    /// Panics if serialization of the fixed freshness schema fails.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("freshness overlay always serializes")
    }

    /// # Errors
    /// Returns the serde error when `bytes` is not a valid encoding.
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// Marks the framed record encoding: a JSON header line, then the raw body bytes.
const RECORD_PREFIX: &[u8] = b"peryx1\n";

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
    /// # Panics
    /// Panics if serialization of the fixed record header fails.
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

#[cfg(test)]
#[path = "../../tests/unit/store/record/tests.rs"]
mod tests;
