//! PEP 740 / index-hosted attestations: parse the upload field, bind each attestation to its
//! distribution, and assemble the provenance object the Simple API serves.
//!
//! Peryx stores what a publisher uploads and serves it back verbatim; it does not verify Sigstore
//! signatures, certificates, or transparency-log inclusion. What it does enforce is the binding a
//! consumer relies on before it ever looks at a signature: every attestation names this exact
//! distribution, by filename and by SHA-256 digest. An attestation that does not is rejected, so a
//! bundle can never claim a file it was not issued for. Untrusted material (the certificate, the
//! transparency entries, the in-toto predicate) is bounded and preserved as opaque JSON, never
//! interpreted.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::view::{AttestationView, SubjectMatch};

/// The media type PEP 740 assigns the served provenance object.
pub const PROVENANCE_MEDIA_TYPE: &str = "application/vnd.pypi.integrity.v1+json";

/// The suffix a distribution's provenance shares with its download URL, mirroring the `.metadata`
/// PEP 658 sibling: `files/{sha256}/{filename}.provenance`.
pub const PROVENANCE_SUFFIX: &str = ".provenance";

/// The provenance and attestation object schema version peryx accepts.
const SUPPORTED_VERSION: u64 = 1;

/// The most attestations one upload may carry for a single distribution. A publisher signs a file a
/// handful of times (one identity, maybe a second for a re-sign), so a bundle in the hundreds is a
/// malformed or hostile request, not a real one.
const MAX_ATTESTATIONS: usize = 32;

/// The largest a single attestation may serialize to. A certificate plus one transparency entry is a
/// few kilobytes; the cap leaves generous room while bounding what one array element can cost.
const MAX_ATTESTATION_BYTES: usize = 256 * 1024;

/// The largest an in-toto statement may decode to. Its size is dominated by the subject list, which
/// for a distribution names one artifact.
const MAX_STATEMENT_BYTES: usize = 64 * 1024;

/// Every variant rejects the upload because attestations publish atomically with the distribution.
#[derive(Debug, PartialEq, Eq)]
pub enum AttestationError {
    /// The field is not a JSON array of attestation objects, or nests past the parser's depth limit.
    Malformed(String),
    /// The array exceeds `MAX_ATTESTATIONS`.
    TooMany(usize),
    /// The field held no attestations; an empty array carries nothing to publish.
    Empty,
    /// One attestation exceeds `MAX_ATTESTATION_BYTES`.
    TooLarge { index: usize, size: usize },
    /// One attestation is not a JSON object.
    NotObject(usize),
    /// One attestation declares a `version` peryx does not implement.
    UnsupportedVersion { index: usize, version: String },
    /// One attestation is missing its DSSE `envelope.statement`.
    MissingStatement(usize),
    /// One attestation's `envelope.statement` is not valid base64.
    InvalidStatementEncoding(usize),
    /// A decoded statement exceeds `MAX_STATEMENT_BYTES` or is not a valid in-toto statement.
    MalformedStatement(usize),
    /// A statement names no subject, so it binds to nothing.
    EmptySubject(usize),
    /// No subject digest matches the distribution's SHA-256.
    SubjectDigestMismatch(usize),
    /// A subject matches the distribution digest but names a different file.
    SubjectNameMismatch {
        index: usize,
        expected: String,
        actual: String,
    },
}

impl AttestationError {
    /// The 400 body a rejected upload returns, naming the offending attestation and the reason so a
    /// publisher can fix the bundle without guessing.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Malformed(reason) => format!("attestations field is not a valid JSON array: {reason}"),
            Self::TooMany(count) => {
                format!("attestations field carries {count} attestations; at most {MAX_ATTESTATIONS} are accepted")
            }
            Self::Empty => "attestations field is an empty array".to_owned(),
            Self::TooLarge { index, size } => {
                format!("attestation {index} is {size} bytes; at most {MAX_ATTESTATION_BYTES} are accepted")
            }
            Self::NotObject(index) => format!("attestation {index} is not a JSON object"),
            Self::UnsupportedVersion { index, version } => {
                format!("attestation {index} declares unsupported version {version}; only version 1 is accepted")
            }
            Self::MissingStatement(index) => format!("attestation {index} is missing its envelope statement"),
            Self::InvalidStatementEncoding(index) => {
                format!("attestation {index} envelope statement is not valid base64")
            }
            Self::MalformedStatement(index) => {
                format!("attestation {index} envelope statement is not a valid in-toto statement")
            }
            Self::EmptySubject(index) => format!("attestation {index} statement names no subject"),
            Self::SubjectDigestMismatch(index) => {
                format!("attestation {index} subject digest does not match the uploaded distribution")
            }
            Self::SubjectNameMismatch {
                index,
                expected,
                actual,
            } => format!("attestation {index} subject names {actual:?} but the distribution is {expected:?}"),
        }
    }
}

/// A validated attestation bundle: the provenance object peryx stores and serves, and the set of
/// in-toto predicate types the bundle carries, which a required-attestation policy matches against.
#[derive(Debug, PartialEq, Eq)]
pub struct BuiltProvenance {
    pub document: Vec<u8>,
    pub predicate_types: BTreeSet<String>,
}

/// Parse and bind the `attestations` upload field for the distribution `filename` whose content is
/// `sha256`.
///
/// The result carries the provenance object peryx stores and serves and the predicate types the
/// bundle declares. Every attestation must name this exact distribution, so a subject mismatch or a
/// malformed envelope rejects the whole upload before either object is published.
///
/// # Errors
/// Returns [`AttestationError`] when the field is malformed, oversized, over-nested, or carries an
/// attestation whose subject does not bind to `sha256` and `filename`.
pub fn build_provenance(raw: &str, sha256: &str, filename: &str) -> Result<BuiltProvenance, AttestationError> {
    let attestations = parse_attestations(raw)?;
    let mut predicate_types = BTreeSet::new();
    for (index, attestation) in attestations.iter().enumerate() {
        if let Some(predicate_type) = validate_attestation(index, attestation, sha256, filename)? {
            predicate_types.insert(predicate_type);
        }
    }
    Ok(BuiltProvenance {
        document: provenance_document(&attestations),
        predicate_types,
    })
}

/// The longest predicate-type text a summary carries. A real in-toto `predicateType` is a short URL;
/// the cap keeps a hostile document from bloating the response and the rendered page.
const MAX_PREDICATE_TYPE_CHARS: usize = 256;

/// Summarize a stored PEP 740 provenance document into the neutral per-attestation view the package
/// page renders, one record per attestation across every bundle.
///
/// This reads the document peryx already stored - it fetches nothing and verifies no signature. It
/// decodes each DSSE statement only far enough to read its `predicateType` and check that a subject
/// digest binds to `sha256`, mirroring the binding [`build_provenance`] enforced at upload.
///
/// Returns `None` when the document does not parse as a version-1 provenance object or carries no
/// attestation, so a caller renders it as an unreadable record rather than an empty panel.
#[must_use]
pub fn summarize_provenance(document: &[u8], sha256: &str, filename: &str) -> Option<Vec<AttestationView>> {
    let stored: StoredProvenance = serde_json::from_slice(document).ok()?;
    if stored.version != SUPPORTED_VERSION {
        return None;
    }
    let summaries: Vec<AttestationView> = stored
        .attestation_bundles
        .into_iter()
        .flat_map(|bundle| bundle.attestations)
        .take(MAX_ATTESTATIONS)
        .map(|attestation| summarize_attestation(&attestation, sha256, filename))
        .collect();
    (!summaries.is_empty()).then_some(summaries)
}

fn summarize_attestation(attestation: &Value, sha256: &str, filename: &str) -> AttestationView {
    let Some(statement) = attestation["envelope"]["statement"]
        .as_str()
        .and_then(|encoded| STANDARD.decode(encoded).ok())
        .filter(|decoded| decoded.len() <= MAX_STATEMENT_BYTES)
        .and_then(|decoded| serde_json::from_slice::<Statement>(&decoded).ok())
    else {
        return AttestationView {
            predicate_type: None,
            subject: SubjectMatch::Unknown,
        };
    };
    AttestationView {
        predicate_type: statement
            .predicate_type
            .map(|predicate| predicate.chars().take(MAX_PREDICATE_TYPE_CHARS).collect()),
        subject: subject_match(&statement.subject, sha256, filename),
    }
}

fn subject_match(subjects: &[Subject], sha256: &str, filename: &str) -> SubjectMatch {
    if subjects.is_empty() {
        return SubjectMatch::Unknown;
    }
    subjects
        .iter()
        .find(|subject| subject.digest.get("sha256").is_some_and(|digest| digest == sha256))
        .map_or(SubjectMatch::Mismatched, |subject| match &subject.name {
            Some(name) if name != filename => SubjectMatch::Mismatched,
            _ => SubjectMatch::Matched,
        })
}

#[derive(serde::Deserialize)]
struct StoredProvenance {
    #[serde(default)]
    version: u64,
    #[serde(default)]
    attestation_bundles: Vec<StoredBundle>,
}

#[derive(serde::Deserialize)]
struct StoredBundle {
    #[serde(default)]
    attestations: Vec<Value>,
}

fn provenance_document(attestations: &[Value]) -> Vec<u8> {
    let document = json!({
        "version": SUPPORTED_VERSION,
        "attestation_bundles": [{
            // Peryx does not resolve the uploader to a Trusted Publisher identity, so the bundle
            // carries no publisher. PEP 740 makes the field nullable for exactly this case.
            "publisher": Value::Null,
            "attestations": attestations,
        }],
    });
    serde_json::to_vec(&document).expect("a provenance document of owned JSON always serializes")
}

fn parse_attestations(raw: &str) -> Result<Vec<Value>, AttestationError> {
    let attestations: Vec<Value> =
        serde_json::from_str(raw).map_err(|err| AttestationError::Malformed(err.to_string()))?;
    if attestations.len() > MAX_ATTESTATIONS {
        return Err(AttestationError::TooMany(attestations.len()));
    }
    if attestations.is_empty() {
        return Err(AttestationError::Empty);
    }
    for (index, attestation) in attestations.iter().enumerate() {
        if !attestation.is_object() {
            return Err(AttestationError::NotObject(index));
        }
        let size = serde_json::to_vec(attestation)
            .expect("a parsed JSON value re-serializes")
            .len();
        if size > MAX_ATTESTATION_BYTES {
            return Err(AttestationError::TooLarge { index, size });
        }
    }
    Ok(attestations)
}

fn validate_attestation(
    index: usize,
    attestation: &Value,
    sha256: &str,
    filename: &str,
) -> Result<Option<String>, AttestationError> {
    match &attestation["version"] {
        Value::Number(version) if version.as_u64() == Some(SUPPORTED_VERSION) => {}
        version => {
            return Err(AttestationError::UnsupportedVersion {
                index,
                version: version.to_string(),
            });
        }
    }
    let statement = decode_statement(index, attestation)?;
    bind_subject(index, &statement, sha256, filename)?;
    Ok(statement.predicate_type)
}

fn decode_statement(index: usize, attestation: &Value) -> Result<Statement, AttestationError> {
    let encoded = attestation["envelope"]["statement"]
        .as_str()
        .ok_or(AttestationError::MissingStatement(index))?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| AttestationError::InvalidStatementEncoding(index))?;
    if decoded.len() > MAX_STATEMENT_BYTES {
        return Err(AttestationError::MalformedStatement(index));
    }
    serde_json::from_slice(&decoded).map_err(|_| AttestationError::MalformedStatement(index))
}

fn bind_subject(index: usize, statement: &Statement, sha256: &str, filename: &str) -> Result<(), AttestationError> {
    if statement.subject.is_empty() {
        return Err(AttestationError::EmptySubject(index));
    }
    let matched = statement
        .subject
        .iter()
        .find(|subject| subject.digest.get("sha256").is_some_and(|digest| digest == sha256))
        .ok_or(AttestationError::SubjectDigestMismatch(index))?;
    match &matched.name {
        Some(name) if name != filename => Err(AttestationError::SubjectNameMismatch {
            index,
            expected: filename.to_owned(),
            actual: name.clone(),
        }),
        _ => Ok(()),
    }
}

#[derive(serde::Deserialize)]
struct Statement {
    subject: Vec<Subject>,
    #[serde(rename = "predicateType", default)]
    predicate_type: Option<String>,
}

#[derive(serde::Deserialize)]
struct Subject {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    digest: BTreeMap<String, String>,
}

#[cfg(test)]
#[path = "../tests/unit/attestation/tests.rs"]
mod tests;
