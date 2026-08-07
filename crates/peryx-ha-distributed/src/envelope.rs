//! A versioned envelope that wraps one replication operation with the provenance a follower needs
//! before it interprets the operation: its schema version, source identity, authority epoch,
//! operation type, an optional W3C trace context, and the ordered [`Change`] itself.
//!
//! The envelope extends the version-1 [`Change`] rather than opening a second replication stream:
//! its ordering is the same journal serial the availability contract expresses staleness and
//! recovery over, and its identity is the `(source, epoch, serial)` triple, stable across replay
//! and idempotent to apply.
//!
//! Two rules govern how the envelope decodes untrusted input. The *unknown-field rule*: decoding
//! ignores fields it does not recognize, so a later producer that adds a field stays readable by
//! this build. The *required-version rule*: decoding rejects any `schema_version` other than
//! [`SCHEMA_VERSION`], the one schema this build speaks, so a consumer never guesses at a schema it
//! cannot model. Untrusted peer bytes are bounded by [`DecodeLimits`] before parsing, so envelope
//! decoding cannot be turned into a blob transport or a stack-exhaustion vector.

use std::fmt;

pub use peryx_ha::{AuthorityEpoch, OperationKind};
use serde::{Deserialize, Serialize};

use crate::protocol::Change;

/// The one envelope schema version this build produces and accepts on decode.
pub const SCHEMA_VERSION: SchemaVersion = SchemaVersion(1);
/// The default untrusted-decode bounds: a metadata envelope, never a blob channel.
pub const DEFAULT_DECODE_LIMITS: DecodeLimits = DecodeLimits {
    max_bytes: 1 << 20,
    max_depth: 32,
};

/// The wire schema version of an [`OperationEnvelope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(pub u16);

impl fmt::Display for SchemaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "v{}", self.0)
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
            schema_version: SCHEMA_VERSION,
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
    /// a non-empty source, a schema version equal to [`SCHEMA_VERSION`], and a well-formed
    /// traceparent when a trace context is present. Unrecognized fields are ignored, so a later
    /// producer that adds a field stays readable.
    ///
    /// # Errors
    /// Returns [`EnvelopeError`] for input past the byte or depth limit, malformed JSON, an empty
    /// source, a schema version other than [`SCHEMA_VERSION`], or a malformed W3C traceparent.
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

/// The parts of a well-formed W3C traceparent a child derivation carries over: everything but the
/// parent-id, which the child replaces with its own span id.
struct TraceparentParts<'a> {
    version: &'a str,
    trace_id: &'a str,
    flags: &'a str,
}

/// Every field is `HEXDIGLC`: lowercase hex only, so an uppercase `A`-`F` is not a valid digit.
fn is_hex_lower(field: &str) -> bool {
    field.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parse `value` into its fields when it is a well-formed W3C traceparent. Beyond the lowercase-hex field
/// shapes this enforces the three values the spec singles out as invalid: an all-zero trace-id, an
/// all-zero parent-id, and the reserved version `ff`. The version is a hex byte, so `0xff` is reserved
/// whichever case its two digits use; the guard compares case-insensitively so an uppercase `FF` cannot
/// slip past the lowercase-hex check on a technicality. Version `00` must end after trace-flags, but a
/// later version may append a non-empty `-`-delimited extension after the 55-character base form and
/// stays valid, so a peer speaking a newer trace-context version still interoperates.
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

/// Whether `value` is a well-formed W3C traceparent.
fn valid_traceparent(value: &str) -> bool {
    parse_traceparent(value).is_some()
}

/// Deriving a child trace context failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TraceError {
    #[error("parent traceparent {0:?} is not a well-formed W3C traceparent")]
    MalformedParent(String),
    #[error("child span id {0:?} must be sixteen hex digits and not all zero")]
    InvalidSpanId(String),
}

/// Derive the child W3C traceparent for an operation that continues the trace `parent` authored.
///
/// The child keeps `parent`'s version, trace-id, and flags and takes `span_id` as its new parent-id, so
/// a follower's apply span joins the trace the ingress opened rather than starting a disconnected one.
/// The caller supplies `span_id`, a fresh sixteen-hex-digit span identifier, which keeps this
/// deterministic.
///
/// # Errors
/// Returns [`TraceError::MalformedParent`] when `parent` is not a valid traceparent, including a
/// reserved `ff` version, and [`TraceError::InvalidSpanId`] when `span_id` is not sixteen hex digits or
/// is all zero.
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

/// Whether `span_id` is a well-formed traceparent parent-id: sixteen hex digits, not all zero.
fn valid_span_id(span_id: &str) -> bool {
    span_id.len() == 16
        && span_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && span_id.bytes().any(|byte| byte != b'0')
}
