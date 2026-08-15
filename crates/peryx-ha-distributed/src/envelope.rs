//! Preserves [`Change`] journal ordering and identifies operations by `(source, epoch, serial)` across
//! replay. Decoding ignores unknown fields, rejects unsupported schema versions, and enforces byte and
//! nesting limits before parsing untrusted input.

use std::fmt;

pub use peryx_ha::{AuthorityEpoch, OperationKind};
use serde::{Deserialize, Serialize};

use crate::protocol::Change;

pub const SCHEMA_VERSION: SchemaVersion = SchemaVersion(1);
/// Bounds untrusted input to metadata-sized messages.
pub const DEFAULT_DECODE_LIMITS: DecodeLimits = DecodeLimits {
    max_bytes: 1 << 20,
    max_depth: 32,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(pub u16);

impl fmt::Display for SchemaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "v{}", self.0)
    }
}

/// Carries W3C [trace context](https://www.w3.org/TR/trace-context/) from authoring to apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub traceparent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

/// A replay-stable operation identity whose display form contains no payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationId<'envelope> {
    pub source: &'envelope str,
    pub epoch: AuthorityEpoch,
    pub serial: u64,
}

impl fmt::Display for OperationId<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}#{}", self.source, self.epoch.0, self.serial)
    }
}

/// Byte and nesting limits applied before parsing untrusted peer input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    pub max_bytes: usize,
    pub max_depth: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        DEFAULT_DECODE_LIMITS
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("replication envelope is {actual} bytes, over the {limit} byte decode limit")]
    TooLarge { limit: usize, actual: usize },
    #[error("replication envelope nests past the {limit} level decode limit")]
    TooDeep { limit: usize },
    #[error("replication envelope is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("unsupported envelope schema version {version}; this build accepts {expected}")]
    UnsupportedVersion {
        version: SchemaVersion,
        expected: SchemaVersion,
    },
    #[error("replication envelope has an empty source identity")]
    EmptySource,
    #[error("replication envelope carries a malformed W3C traceparent {0:?}")]
    InvalidTrace(String),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEnvelope {
    pub schema_version: SchemaVersion,
    pub source: String,
    pub epoch: AuthorityEpoch,
    pub kind: OperationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceContext>,
    pub change: Change,
}

impl OperationEnvelope {
    #[must_use]
    pub fn current(source: impl Into<String>, epoch: AuthorityEpoch, kind: OperationKind, change: Change) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            source: source.into(),
            epoch,
            kind,
            trace: None,
            change,
        }
    }

    /// Returns the replay-stable, payload-free identity used in logs.
    #[must_use]
    pub fn identity(&self) -> OperationId<'_> {
        OperationId {
            source: &self.source,
            epoch: self.epoch,
            serial: self.change.serial,
        }
    }

    /// # Panics
    /// Panics if serde cannot serialize the envelope's fixed field types.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("an operation envelope always serializes to JSON")
    }

    /// Enforces byte and nesting limits before parsing. Accepted envelopes have a non-empty source,
    /// [`SCHEMA_VERSION`], and a valid traceparent when present. Unknown fields remain compatible.
    ///
    /// # Errors
    /// Returns [`EnvelopeError`] for limit violations, malformed JSON, an empty source, an unsupported
    /// schema version, or a malformed W3C traceparent.
    pub fn decode(bytes: &[u8], limits: DecodeLimits) -> Result<Self, EnvelopeError> {
        if bytes.len() > limits.max_bytes {
            return Err(EnvelopeError::TooLarge {
                limit: limits.max_bytes,
                actual: bytes.len(),
            });
        }
        if exceeds_depth(bytes, limits.max_depth) {
            return Err(EnvelopeError::TooDeep {
                limit: limits.max_depth,
            });
        }
        let envelope: Self = serde_json::from_slice(bytes).map_err(EnvelopeError::Malformed)?;
        envelope.validated()
    }

    fn validated(self) -> Result<Self, EnvelopeError> {
        if self.source.is_empty() {
            return Err(EnvelopeError::EmptySource);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(EnvelopeError::UnsupportedVersion {
                version: self.schema_version,
                expected: SCHEMA_VERSION,
            });
        }
        if let Some(trace) = &self.trace
            && !valid_traceparent(&trace.traceparent)
        {
            return Err(EnvelopeError::InvalidTrace(trace.traceparent.clone()));
        }
        Ok(self)
    }
}

impl fmt::Debug for OperationEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationEnvelope")
            .field("schema_version", &self.schema_version)
            .field("source", &self.source)
            .field("epoch", &self.epoch)
            .field("kind", &self.kind)
            .field("serial", &self.change.serial)
            .field("traceparent", &self.trace.as_ref().map(|trace| &trace.traceparent))
            .finish_non_exhaustive()
    }
}

impl fmt::Display for OperationEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {} {}", self.kind, self.schema_version, self.identity())
    }
}

fn exceeds_depth(bytes: &[u8], max: usize) -> bool {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            match (escaped, byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                (false, _) => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

struct TraceparentParts<'a> {
    version: &'a str,
    trace_id: &'a str,
    flags: &'a str,
}

/// W3C `HEXDIGLC` excludes uppercase hexadecimal digits.
fn is_hex_lower(field: &str) -> bool {
    field.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Rejects all-zero IDs and reserved version `ff`. Version `00` cannot carry extensions; later
/// versions may append a non-empty extension to preserve forward compatibility.
fn parse_traceparent(value: &str) -> Option<TraceparentParts<'_>> {
    let mut fields = value.splitn(5, '-');
    let (Some(version), Some(trace_id), Some(parent_id), Some(flags)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    let base_valid = version.len() == 2
        && !version.eq_ignore_ascii_case("ff")
        && trace_id.len() == 32
        && parent_id.len() == 16
        && flags.len() == 2
        && is_hex_lower(version)
        && is_hex_lower(trace_id)
        && is_hex_lower(parent_id)
        && is_hex_lower(flags)
        && trace_id.bytes().any(|byte| byte != b'0')
        && parent_id.bytes().any(|byte| byte != b'0');
    let extension_valid = fields
        .next()
        .is_none_or(|extension| version != "00" && !extension.is_empty());
    (base_valid && extension_valid).then_some(TraceparentParts {
        version,
        trace_id,
        flags,
    })
}

fn valid_traceparent(value: &str) -> bool {
    parse_traceparent(value).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TraceError {
    #[error("parent traceparent {0:?} is not a well-formed W3C traceparent")]
    MalformedParent(String),
    #[error("child span id {0:?} must be sixteen hex digits and not all zero")]
    InvalidSpanId(String),
}

/// Preserves the parent's version, trace ID, and flags while replacing its parent ID with `span_id`.
/// Callers must supply a fresh 16-digit hexadecimal span ID.
///
/// # Errors
/// Returns [`TraceError::MalformedParent`] for an invalid traceparent and
/// [`TraceError::InvalidSpanId`] for a malformed or all-zero span ID.
pub fn derive_child(parent: &str, span_id: &str) -> Result<String, TraceError> {
    let Some(parts) = parse_traceparent(parent) else {
        return Err(TraceError::MalformedParent(parent.to_owned()));
    };
    if !valid_span_id(span_id) {
        return Err(TraceError::InvalidSpanId(span_id.to_owned()));
    }
    Ok(format!(
        "{}-{}-{span_id}-{}",
        parts.version, parts.trace_id, parts.flags
    ))
}

fn valid_span_id(span_id: &str) -> bool {
    span_id.len() == 16
        && span_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && span_id.bytes().any(|byte| byte != b'0')
}
