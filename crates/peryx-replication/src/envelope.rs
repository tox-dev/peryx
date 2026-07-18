//! A versioned envelope that wraps one replication operation with the provenance a follower needs
//! before it interprets the operation: its schema version, source identity, authority epoch,
//! operation type, an optional W3C trace context, and the ordered [`Change`] itself.
//!
//! The envelope extends the version-1 [`Change`] rather than opening a second replication stream:
//! its ordering is the same journal serial the availability contract expresses staleness and
//! recovery over, and its identity is the `(source, epoch, serial)` triple, stable across replay
//! and idempotent to apply.
//!
//! Two compatibility rules keep a rolling upgrade between adjacent schema versions safe. The
//! *unknown-field rule*: decoding ignores fields it does not recognize, so a newer producer that
//! adds a field stays readable by an older consumer within the supported window. The
//! *required-version rule*: decoding rejects any `schema_version` outside
//! [`SUPPORTED_SCHEMA_VERSIONS`], so a consumer never guesses at a schema it cannot model. Untrusted
//! peer bytes are bounded by [`DecodeLimits`] before parsing, so envelope decoding cannot be turned
//! into a blob transport or a stack-exhaustion vector.

use std::fmt;
use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

use crate::protocol::Change;

/// The oldest envelope schema version this build can decode.
pub const MIN_SCHEMA_VERSION: SchemaVersion = SchemaVersion(1);
/// The envelope schema version this build produces.
pub const CURRENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion(1);
/// The inclusive range of schema versions this build accepts on decode.
pub const SUPPORTED_SCHEMA_VERSIONS: RangeInclusive<SchemaVersion> = MIN_SCHEMA_VERSION..=CURRENT_SCHEMA_VERSION;
/// The default untrusted-decode bounds: a metadata envelope, never a blob channel.
pub const DEFAULT_DECODE_LIMITS: DecodeLimits = DecodeLimits {
    max_bytes: 1 << 20,
    max_depth: 32,
};

/// The wire schema version of an [`OperationEnvelope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(pub u16);

impl SchemaVersion {
    /// The highest version both ends support, or `None` when their ranges do not overlap.
    ///
    /// This is the version-negotiation surface: a producer and consumer each advertise the inclusive
    /// range they can speak, and the exchange proceeds at the newest version common to both.
    #[must_use]
    pub fn negotiate(local: RangeInclusive<Self>, remote: RangeInclusive<Self>) -> Option<Self> {
        let low = *local.start().max(remote.start());
        let high = *local.end().min(remote.end());
        (low <= high).then_some(high)
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "v{}", self.0)
    }
}

/// The generation of the authoritative primary that produced an operation.
///
/// A failover advances the epoch, so a follower fences a stale primary by refusing an operation
/// whose epoch precedes the one it has already accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthorityEpoch(pub u64);

/// The kind of mutation an operation carries, the one field a log line may always expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Upload,
    Yank,
    Delete,
    CacheFill,
    OciPush,
    OciDelete,
}

impl OperationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Yank => "yank",
            Self::Delete => "delete",
            Self::CacheFill => "cache-fill",
            Self::OciPush => "oci-push",
            Self::OciDelete => "oci-delete",
        }
    }
}

impl fmt::Display for OperationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// W3C [trace context](https://www.w3.org/TR/trace-context/) propagated with an operation so a
/// follower's apply span joins the trace that authored it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub traceparent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

/// The `(source, epoch, serial)` identity of an operation, unique per producer and idempotent to
/// apply. Rendered for logs, it carries no payload bytes.
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

/// The size and nesting bounds applied to untrusted peer bytes before they are parsed.
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

/// An encode, decode-limit, or compatibility-rule failure on the envelope boundary.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("replication envelope is {actual} bytes, over the {limit} byte decode limit")]
    TooLarge { limit: usize, actual: usize },
    #[error("replication envelope nests past the {limit} level decode limit")]
    TooDeep { limit: usize },
    #[error("replication envelope is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("unsupported envelope schema version {version}; this build accepts {min} through {max}")]
    UnsupportedVersion {
        version: SchemaVersion,
        min: SchemaVersion,
        max: SchemaVersion,
    },
    #[error("replication envelope has an empty source identity")]
    EmptySource,
    #[error("replication envelope carries a malformed W3C traceparent {0:?}")]
    InvalidTrace(String),
}

/// A versioned replication operation: a [`Change`] wrapped with its schema version, source,
/// authority epoch, operation kind, and optional trace context.
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
    /// Wrap `change` at the current schema version, without a trace context.
    #[must_use]
    pub fn current(source: impl Into<String>, epoch: AuthorityEpoch, kind: OperationKind, change: Change) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            source: source.into(),
            epoch,
            kind,
            trace: None,
            change,
        }
    }

    /// The operation's log-safe `(source, epoch, serial)` identity.
    #[must_use]
    pub fn identity(&self) -> OperationId<'_> {
        OperationId {
            source: &self.source,
            epoch: self.epoch,
            serial: self.change.serial,
        }
    }

    /// Serialize the envelope to its JSON wire form.
    ///
    /// # Panics
    /// Panics only if serializing to JSON fails, which the envelope's field types make unreachable.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("an operation envelope always serializes to JSON")
    }

    /// Parse an envelope from untrusted peer bytes under `limits`.
    ///
    /// Oversized or over-nested input is rejected before parsing; a decoded envelope must then carry
    /// a non-empty source, a schema version within [`SUPPORTED_SCHEMA_VERSIONS`], and a well-formed
    /// traceparent when a trace context is present. Unrecognized fields are ignored, so an adjacent
    /// newer schema stays readable.
    ///
    /// # Errors
    /// Returns [`EnvelopeError`] for input past the byte or depth limit, malformed JSON, an empty
    /// source, an out-of-window schema version, or a malformed W3C traceparent.
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
        if !SUPPORTED_SCHEMA_VERSIONS.contains(&self.schema_version) {
            return Err(EnvelopeError::UnsupportedVersion {
                version: self.schema_version,
                min: MIN_SCHEMA_VERSION,
                max: CURRENT_SCHEMA_VERSION,
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

fn valid_traceparent(value: &str) -> bool {
    let mut fields = value.split('-');
    let (Some(version), Some(trace_id), Some(parent_id), Some(flags), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return false;
    };
    version.len() == 2
        && trace_id.len() == 32
        && parent_id.len() == 16
        && flags.len() == 2
        && [version, trace_id, parent_id, flags]
            .iter()
            .all(|field| field.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && trace_id.bytes().any(|byte| byte != b'0')
        && parent_id.bytes().any(|byte| byte != b'0')
}
