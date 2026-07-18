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

/// Why an uploaded `attestations` field was rejected. Every variant is a client error: the upload,
/// and the distribution it rode in on, publish only when every attestation validates.
#[derive(Debug, PartialEq, Eq)]
pub enum AttestationError {
    /// The field is not a JSON array of attestation objects, or nests past the parser's depth limit.
    Malformed(String),
    /// The array holds more attestations than [`MAX_ATTESTATIONS`].
    TooMany(usize),
    /// The field held no attestations; an empty array carries nothing to publish.
    Empty,
    /// One attestation serializes past [`MAX_ATTESTATION_BYTES`].
    TooLarge { index: usize, size: usize },
    /// One attestation is not a JSON object.
    NotObject(usize),
    /// One attestation declares a `version` peryx does not implement.
    UnsupportedVersion { index: usize, version: String },
    /// One attestation is missing its DSSE `envelope.statement`.
    MissingStatement(usize),
    /// One attestation's `envelope.statement` is not valid base64.
    InvalidStatementEncoding(usize),
    /// A decoded statement exceeds [`MAX_STATEMENT_BYTES`] or is not a valid in-toto statement.
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

/// Validate one attestation and, on success, return the in-toto predicate type it declares, when the
/// statement carries one.
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
mod tests {
    use super::*;

    const SHA: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";

    fn statement(name: &str, sha: &str) -> String {
        STANDARD.encode(
            json!({
                "_type": "https://in-toto.io/Statement/v1",
                "subject": [{"name": name, "digest": {"sha256": sha}}],
                "predicateType": "https://docs.pypi.org/attestations/publish/v1",
                "predicate": {},
            })
            .to_string(),
        )
    }

    fn attestation(name: &str, sha: &str) -> Value {
        json!({
            "version": 1,
            "verification_material": {"certificate": "Zm9v", "transparency_entries": []},
            "envelope": {"statement": statement(name, sha), "signature": "YmFy"},
        })
    }

    fn field(attestations: &[Value]) -> String {
        serde_json::to_string(attestations).unwrap()
    }

    #[test]
    fn test_build_provenance_wraps_a_bound_attestation() {
        let raw = field(&[attestation(FILENAME, SHA)]);

        let built = build_provenance(&raw, SHA, FILENAME).unwrap();
        let document: Value = serde_json::from_slice(&built.document).unwrap();

        assert_eq!(document["version"], 1);
        let bundle = &document["attestation_bundles"][0];
        assert_eq!(bundle["publisher"], Value::Null);
        assert_eq!(bundle["attestations"].as_array().unwrap().len(), 1);
        assert_eq!(bundle["attestations"][0]["version"], 1);
    }

    #[test]
    fn test_build_provenance_preserves_untrusted_material_verbatim() {
        let raw = field(&[attestation(FILENAME, SHA)]);

        let document: Value = serde_json::from_slice(&build_provenance(&raw, SHA, FILENAME).unwrap().document).unwrap();

        let material = &document["attestation_bundles"][0]["attestations"][0]["verification_material"];
        assert_eq!(material["certificate"], "Zm9v");
    }

    #[test]
    fn test_build_provenance_collects_the_declared_predicate_types() {
        let raw = field(&[attestation(FILENAME, SHA)]);

        let built = build_provenance(&raw, SHA, FILENAME).unwrap();

        assert_eq!(
            built.predicate_types,
            BTreeSet::from(["https://docs.pypi.org/attestations/publish/v1".to_owned()])
        );
    }

    #[test]
    fn test_build_provenance_rejects_a_non_array() {
        let error = build_provenance("{}", SHA, FILENAME).unwrap_err();
        assert!(matches!(error, AttestationError::Malformed(_)));
    }

    #[test]
    fn test_build_provenance_rejects_excessive_nesting() {
        let raw = format!("{}{}", "[".repeat(300), "]".repeat(300));

        let error = build_provenance(&raw, SHA, FILENAME).unwrap_err();

        assert!(matches!(error, AttestationError::Malformed(_)), "{error:?}");
    }

    #[test]
    fn test_build_provenance_rejects_an_empty_array() {
        assert_eq!(
            build_provenance("[]", SHA, FILENAME).unwrap_err(),
            AttestationError::Empty
        );
    }

    #[test]
    fn test_build_provenance_rejects_too_many_attestations() {
        let raw = field(&vec![attestation(FILENAME, SHA); MAX_ATTESTATIONS + 1]);
        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::TooMany(MAX_ATTESTATIONS + 1)
        );
    }

    #[test]
    fn test_build_provenance_rejects_an_oversized_attestation() {
        let mut oversized = attestation(FILENAME, SHA);
        oversized["verification_material"]["certificate"] = json!("A".repeat(MAX_ATTESTATION_BYTES + 1));
        let raw = field(&[oversized]);

        assert!(matches!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::TooLarge { index: 0, .. }
        ));
    }

    #[test]
    fn test_build_provenance_rejects_an_unsupported_version() {
        let mut future = attestation(FILENAME, SHA);
        future["version"] = json!(2);
        let raw = field(&[future]);

        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::UnsupportedVersion {
                index: 0,
                version: "2".to_owned(),
            }
        );
    }

    #[test]
    fn test_build_provenance_rejects_a_missing_statement() {
        let mut missing = attestation(FILENAME, SHA);
        missing["envelope"] = json!({"signature": "YmFy"});
        let raw = field(&[missing]);

        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::MissingStatement(0)
        );
    }

    #[test]
    fn test_build_provenance_rejects_non_base64_statement() {
        let mut bad = attestation(FILENAME, SHA);
        bad["envelope"]["statement"] = json!("not base64!!");
        let raw = field(&[bad]);

        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::InvalidStatementEncoding(0)
        );
    }

    #[test]
    fn test_build_provenance_rejects_a_malformed_statement() {
        let mut bad = attestation(FILENAME, SHA);
        bad["envelope"]["statement"] = json!(STANDARD.encode("not json"));
        let raw = field(&[bad]);

        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::MalformedStatement(0)
        );
    }

    #[test]
    fn test_build_provenance_rejects_an_empty_subject() {
        let mut empty = attestation(FILENAME, SHA);
        empty["envelope"]["statement"] = json!(STANDARD.encode(json!({"subject": []}).to_string()));
        let raw = field(&[empty]);

        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::EmptySubject(0)
        );
    }

    #[test]
    fn test_build_provenance_rejects_a_subject_digest_mismatch() {
        let other = "2222222222222222222222222222222222222222222222222222222222222222";
        let raw = field(&[attestation(FILENAME, other)]);

        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::SubjectDigestMismatch(0)
        );
    }

    #[test]
    fn test_build_provenance_rejects_a_subject_name_mismatch() {
        let raw = field(&[attestation("other-1.0-py3-none-any.whl", SHA)]);

        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::SubjectNameMismatch {
                index: 0,
                expected: FILENAME.to_owned(),
                actual: "other-1.0-py3-none-any.whl".to_owned(),
            }
        );
    }

    #[test]
    fn test_build_provenance_accepts_a_subject_without_a_name() {
        let mut anonymous = attestation(FILENAME, SHA);
        anonymous["envelope"]["statement"] =
            json!(STANDARD.encode(json!({"subject": [{"digest": {"sha256": SHA}}]}).to_string()));
        let raw = field(&[anonymous]);

        assert!(build_provenance(&raw, SHA, FILENAME).is_ok());
    }

    #[test]
    fn test_build_provenance_rejects_an_oversized_statement() {
        let mut oversized = attestation(FILENAME, SHA);
        let subject = json!({"subject": [{"name": "a".repeat(MAX_STATEMENT_BYTES + 1), "digest": {"sha256": SHA}}]});
        oversized["envelope"]["statement"] = json!(STANDARD.encode(subject.to_string()));
        let raw = field(&[oversized]);

        assert_eq!(
            build_provenance(&raw, SHA, FILENAME).unwrap_err(),
            AttestationError::MalformedStatement(0)
        );
    }

    #[test]
    fn test_message_names_the_reason_for_every_variant() {
        for (error, expected) in [
            (AttestationError::Malformed("boom".to_owned()), "valid JSON array"),
            (AttestationError::TooMany(99), "at most 32"),
            (AttestationError::Empty, "empty array"),
            (
                AttestationError::TooLarge { index: 1, size: 5 },
                "attestation 1 is 5 bytes",
            ),
            (AttestationError::NotObject(2), "attestation 2 is not a JSON object"),
            (
                AttestationError::UnsupportedVersion {
                    index: 0,
                    version: "9".to_owned(),
                },
                "unsupported version 9",
            ),
            (AttestationError::MissingStatement(0), "missing its envelope statement"),
            (AttestationError::InvalidStatementEncoding(0), "not valid base64"),
            (AttestationError::MalformedStatement(0), "not a valid in-toto statement"),
            (AttestationError::EmptySubject(0), "names no subject"),
            (
                AttestationError::SubjectDigestMismatch(3),
                "attestation 3 subject digest",
            ),
            (
                AttestationError::SubjectNameMismatch {
                    index: 0,
                    expected: "a.whl".to_owned(),
                    actual: "b.whl".to_owned(),
                },
                "subject names \"b.whl\"",
            ),
        ] {
            assert!(error.message().contains(expected), "{error:?} -> {}", error.message());
        }
    }
}
