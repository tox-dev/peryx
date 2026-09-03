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
fn test_build_provenance_accepts_thirty_two_attestations() {
    let attestations = vec![attestation(FILENAME, SHA); 32];
    let document: Value =
        serde_json::from_slice(&build_provenance(&field(&attestations), SHA, FILENAME).unwrap().document).unwrap();

    assert_eq!(document["attestation_bundles"][0]["attestations"], json!(attestations));
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
fn test_build_provenance_accepts_a_262144_byte_attestation() {
    let mut boundary = attestation(FILENAME, SHA);
    boundary["verification_material"]["certificate"] = json!("");
    let base_size = serde_json::to_vec(&boundary).unwrap().len();
    boundary["verification_material"]["certificate"] = json!("A".repeat(256 * 1024 - base_size));
    assert_eq!(serde_json::to_vec(&boundary).unwrap().len(), 256 * 1024);

    let document: Value = serde_json::from_slice(
        &build_provenance(&field(&[boundary.clone()]), SHA, FILENAME)
            .unwrap()
            .document,
    )
    .unwrap();

    assert_eq!(document["attestation_bundles"][0]["attestations"][0], boundary);
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
fn test_build_provenance_accepts_a_65536_byte_statement() {
    let mut statement = json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{"name": FILENAME, "digest": {"sha256": SHA}}],
        "predicateType": "https://docs.pypi.org/attestations/publish/v1",
        "predicate": {},
        "padding": "",
    });
    let base_size = serde_json::to_vec(&statement).unwrap().len();
    statement["padding"] = json!("A".repeat(64 * 1024 - base_size));
    let statement = serde_json::to_vec(&statement).unwrap();
    assert_eq!(statement.len(), 64 * 1024);
    let mut boundary = attestation(FILENAME, SHA);
    boundary["envelope"]["statement"] = json!(STANDARD.encode(statement));

    let document: Value = serde_json::from_slice(
        &build_provenance(&field(&[boundary.clone()]), SHA, FILENAME)
            .unwrap()
            .document,
    )
    .unwrap();

    assert_eq!(document["attestation_bundles"][0]["attestations"][0], boundary);
}

#[test]
fn test_summarize_provenance_reads_a_bound_attestation() {
    let document = build_provenance(&field(&[attestation(FILENAME, SHA)]), SHA, FILENAME)
        .unwrap()
        .document;

    let summaries = summarize_provenance(&document, SHA, FILENAME).unwrap();

    assert_eq!(
        summaries,
        vec![AttestationView {
            predicate_type: Some("https://docs.pypi.org/attestations/publish/v1".to_owned()),
            subject: SubjectMatch::Matched,
        }]
    );
}

#[test]
fn test_summarize_provenance_records_every_attestation() {
    let document = provenance_document(&[attestation(FILENAME, SHA), attestation(FILENAME, SHA)]);

    let summaries = summarize_provenance(&document, SHA, FILENAME).unwrap();

    assert_eq!(summaries.len(), 2);
    assert!(summaries.iter().all(|summary| summary.subject == SubjectMatch::Matched));
}

#[test]
fn test_summarize_provenance_flags_a_name_mismatch() {
    let document = provenance_document(&[attestation("other-1.0-py3-none-any.whl", SHA)]);

    let summaries = summarize_provenance(&document, SHA, FILENAME).unwrap();

    assert_eq!(summaries[0].subject, SubjectMatch::Mismatched);
}

#[test]
fn test_summarize_provenance_flags_a_digest_mismatch() {
    let other = "2222222222222222222222222222222222222222222222222222222222222222";
    let document = provenance_document(&[attestation(FILENAME, other)]);

    let summaries = summarize_provenance(&document, SHA, FILENAME).unwrap();

    assert_eq!(summaries[0].subject, SubjectMatch::Mismatched);
}

#[test]
fn test_summarize_provenance_reports_unknown_for_an_unreadable_statement() {
    let mut missing = attestation(FILENAME, SHA);
    missing["envelope"] = json!({"signature": "YmFy"});
    let document = provenance_document(&[missing]);

    let summaries = summarize_provenance(&document, SHA, FILENAME).unwrap();

    assert_eq!(
        summaries,
        vec![AttestationView {
            predicate_type: None,
            subject: SubjectMatch::Unknown,
        }]
    );
}

#[test]
fn test_summarize_provenance_reports_unknown_for_an_empty_subject() {
    let mut empty = attestation(FILENAME, SHA);
    empty["envelope"]["statement"] = json!(STANDARD.encode(json!({"subject": []}).to_string()));
    let document = provenance_document(&[empty]);

    let summaries = summarize_provenance(&document, SHA, FILENAME).unwrap();

    assert_eq!(summaries[0].subject, SubjectMatch::Unknown);
}

#[test]
fn test_summarize_provenance_omits_an_absent_predicate_type() {
    let mut anonymous = attestation(FILENAME, SHA);
    anonymous["envelope"]["statement"] =
        json!(STANDARD.encode(json!({"subject": [{"name": FILENAME, "digest": {"sha256": SHA}}]}).to_string()));
    let document = provenance_document(&[anonymous]);

    let summaries = summarize_provenance(&document, SHA, FILENAME).unwrap();

    assert_eq!(summaries[0].predicate_type, None);
    assert_eq!(summaries[0].subject, SubjectMatch::Matched);
}

#[test]
fn test_summarize_provenance_bounds_a_hostile_predicate_type() {
    let mut hostile = attestation(FILENAME, SHA);
    hostile["envelope"]["statement"] = json!(
        STANDARD.encode(
            json!({
                "subject": [{"name": FILENAME, "digest": {"sha256": SHA}}],
                "predicateType": "x".repeat(MAX_PREDICATE_TYPE_CHARS + 50),
            })
            .to_string()
        )
    );
    let document = provenance_document(&[hostile]);

    let predicate = summarize_provenance(&document, SHA, FILENAME).unwrap()[0]
        .predicate_type
        .clone()
        .unwrap();

    assert_eq!(predicate.chars().count(), MAX_PREDICATE_TYPE_CHARS);
}

#[test]
fn test_summarize_provenance_rejects_a_non_provenance_document() {
    assert_eq!(summarize_provenance(b"not json", SHA, FILENAME), None);
    assert_eq!(summarize_provenance(b"{\"version\":2}", SHA, FILENAME), None);
    assert_eq!(
        summarize_provenance(br#"{"version":1,"attestation_bundles":[]}"#, SHA, FILENAME),
        None
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

/// A document peryx cannot read declares no predicate types, so a promotion judges it as carrying no
/// attestation rather than failing on it.
#[rstest::rstest]
#[case::not_json(b"not json".to_vec())]
#[case::wrong_version(serde_json::json!({"version": 99, "attestation_bundles": []}).to_string().into_bytes())]
#[case::no_bundles(serde_json::json!({"version": 1, "attestation_bundles": []}).to_string().into_bytes())]
fn stored_predicate_types_reads_nothing_from_an_unusable_document(#[case] document: Vec<u8>) {
    assert!(crate::attestation::stored_predicate_types(&document, "aa", "pkg-1.0-py3-none-any.whl").is_empty());
}
