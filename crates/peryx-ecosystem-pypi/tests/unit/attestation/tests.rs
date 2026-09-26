use super::*;
use std::sync::Arc;

use rstest::rstest;
use sigstore_verify::Verifier;
use sigstore_verify::trust_root::{SIGSTORE_PRODUCTION_TRUSTED_ROOT, SIGSTORE_STAGING_TRUSTED_ROOT, TrustedRoot};

const SIGNED_FILENAME: &str = "pypi_attestations-0.0.19.tar.gz";
const SIGNED_SHA256: &str = "9bb1add04b1b4e182be6b0b80931593f7a291eb49d69b4fd728a5d4cbcdc4bd3";
const SIGNED_ATTESTATION: &str = include_str!("../../fixtures/pypi_attestations-0.0.19.tar.gz.publish.attestation");
const SIGNED_IDENTITY: &str =
    "https://github.com/trailofbits/pypi-attestations/.github/workflows/release.yml@refs/tags/v0.0.19";

fn build_provenance(raw: &str, sha256: &str, filename: &str) -> Result<BuiltProvenance, AttestationError> {
    build_provenance_unverified(raw, sha256, filename)
}

fn verification_context(root: &str, identity: &str, claims: BTreeMap<String, String>) -> VerificationContext {
    verification_context_with_issuer(root, identity, "https://token.actions.githubusercontent.com", claims)
}

fn verification_context_with_issuer(
    root: &str,
    identity: &str,
    issuer: &str,
    claims: BTreeMap<String, String>,
) -> VerificationContext {
    let root = TrustedRoot::from_json(root).unwrap();
    VerificationContext::new(
        Arc::new(Verifier::new(&root).unwrap()),
        identity.to_owned(),
        issuer.to_owned(),
        claims,
    )
}

fn signed_field(attestation: Value) -> String {
    serde_json::to_string(&[attestation]).unwrap()
}

fn signed_attestation() -> Value {
    serde_json::from_str(SIGNED_ATTESTATION).unwrap()
}

#[test]
fn test_signed_attestation_verifies() {
    let built = super::build_provenance(
        &signed_field(signed_attestation()),
        SIGNED_SHA256,
        SIGNED_FILENAME,
        Some(&verification_context(
            SIGSTORE_PRODUCTION_TRUSTED_ROOT,
            SIGNED_IDENTITY,
            BTreeMap::new(),
        )),
    )
    .unwrap();

    assert_eq!(
        built.predicate_types,
        BTreeSet::from(["https://docs.pypi.org/attestations/publish/v1".to_owned()])
    );
    let document: Value = serde_json::from_slice(&built.document).unwrap();
    assert_eq!(
        document["attestation_bundles"][0]["publisher"],
        json!({
            "kind": "https://token.actions.githubusercontent.com",
            "claims": {"identity": SIGNED_IDENTITY},
        })
    );
}

#[test]
fn test_attestation_without_a_verification_policy_is_rejected() {
    assert_eq!(
        super::build_provenance(
            &signed_field(signed_attestation()),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            None,
        )
        .unwrap_err(),
        AttestationError::MissingVerificationPolicy
    );
}

#[test]
fn test_signed_attestation_with_an_unsupported_version_is_rejected() {
    let mut attestation = signed_attestation();
    attestation["version"] = json!(2);

    assert_eq!(
        super::build_provenance(
            &signed_field(attestation),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context(
                SIGSTORE_PRODUCTION_TRUSTED_ROOT,
                SIGNED_IDENTITY,
                BTreeMap::new(),
            )),
        )
        .unwrap_err(),
        AttestationError::UnsupportedVersion {
            index: 0,
            version: "2".to_owned(),
        }
    );
}

#[rstest]
#[case::signature("envelope", "signature")]
#[case::certificate("verification_material", "certificate")]
fn test_mutated_signed_material_is_rejected(#[case] object: &str, #[case] field: &str) {
    let mut attestation = signed_attestation();
    let value = attestation[object][field].as_str().unwrap();
    attestation[object][field] = Value::String(format!("A{}", &value[1..]));

    assert_eq!(
        super::build_provenance(
            &signed_field(attestation),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context(
                SIGSTORE_PRODUCTION_TRUSTED_ROOT,
                SIGNED_IDENTITY,
                BTreeMap::new(),
            )),
        )
        .unwrap_err(),
        AttestationError::VerificationFailed(0)
    );
}

#[test]
fn test_mutated_statement_is_rejected() {
    let mut attestation = signed_attestation();
    let statement = STANDARD
        .decode(attestation["envelope"]["statement"].as_str().unwrap())
        .unwrap();
    let mut statement: Value = serde_json::from_slice(&statement).unwrap();
    statement["predicate"] = serde_json::json!({"changed": true});
    attestation["envelope"]["statement"] = Value::String(STANDARD.encode(serde_json::to_vec(&statement).unwrap()));

    assert_eq!(
        super::build_provenance(
            &signed_field(attestation),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context(
                SIGSTORE_PRODUCTION_TRUSTED_ROOT,
                SIGNED_IDENTITY,
                BTreeMap::new(),
            )),
        )
        .unwrap_err(),
        AttestationError::VerificationFailed(0)
    );
}

#[rstest]
#[case::identity("other@example.com", SIGSTORE_PRODUCTION_TRUSTED_ROOT)]
#[case::root(SIGNED_IDENTITY, SIGSTORE_STAGING_TRUSTED_ROOT)]
fn test_untrusted_attestation_identity_or_root_is_rejected(#[case] identity: &str, #[case] root: &str) {
    assert_eq!(
        super::build_provenance(
            &signed_field(signed_attestation()),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context(root, identity, BTreeMap::new())),
        )
        .unwrap_err(),
        AttestationError::VerificationFailed(0)
    );
}

#[test]
fn test_untrusted_attestation_issuer_is_rejected() {
    assert_eq!(
        super::build_provenance(
            &signed_field(signed_attestation()),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context_with_issuer(
                SIGSTORE_PRODUCTION_TRUSTED_ROOT,
                SIGNED_IDENTITY,
                "https://issuer.example",
                BTreeMap::new(),
            )),
        )
        .unwrap_err(),
        AttestationError::VerificationFailed(0)
    );
}

#[test]
fn test_attestation_without_transparency_evidence_is_rejected() {
    let mut attestation = signed_attestation();
    attestation["verification_material"]["transparency_entries"] = json!([]);

    assert_eq!(
        super::build_provenance(
            &signed_field(attestation),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context(
                SIGSTORE_PRODUCTION_TRUSTED_ROOT,
                SIGNED_IDENTITY,
                BTreeMap::new(),
            )),
        )
        .unwrap_err(),
        AttestationError::VerificationFailed(0)
    );
}

#[test]
fn test_matching_certificate_claim_is_accepted() {
    assert!(
        super::build_provenance(
            &signed_field(signed_attestation()),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context(
                SIGSTORE_PRODUCTION_TRUSTED_ROOT,
                SIGNED_IDENTITY,
                BTreeMap::from([(
                    "1.3.6.1.4.1.57264.1.12".to_owned(),
                    "https://github.com/trailofbits/pypi-attestations".to_owned(),
                )]),
            )),
        )
        .is_ok()
    );
}

#[test]
fn test_mismatched_certificate_claim_is_rejected() {
    assert_eq!(
        super::build_provenance(
            &signed_field(signed_attestation()),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context(
                SIGSTORE_PRODUCTION_TRUSTED_ROOT,
                SIGNED_IDENTITY,
                BTreeMap::from([(
                    "1.3.6.1.4.1.57264.1.12".to_owned(),
                    "https://github.com/other/repo".to_owned()
                )]),
            )),
        )
        .unwrap_err(),
        AttestationError::VerificationFailed(0)
    );
}

#[rstest]
#[case::integrated_time("integrated-time")]
#[case::log_key("log-key")]
#[case::set("set")]
#[case::missing_proof("missing-proof")]
#[case::checkpoint("checkpoint")]
#[case::body("body")]
fn test_mutated_transparency_material_is_rejected(#[case] mutation: &str) {
    let mut attestation = signed_attestation();
    let entry = &mut attestation["verification_material"]["transparency_entries"][0];
    match mutation {
        "integrated-time" => entry["integratedTime"] = Value::String("1733354042".to_owned()),
        "log-key" => entry["logId"]["keyId"] = Value::String(STANDARD.encode([0; 32])),
        "set" => entry["inclusionPromise"]["signedEntryTimestamp"] = Value::String("AA==".to_owned()),
        "missing-proof" => {
            entry.as_object_mut().unwrap().remove("inclusionProof");
        }
        "checkpoint" => entry["inclusionProof"]["checkpoint"]["envelope"] = Value::String("changed".to_owned()),
        "body" => entry["canonicalizedBody"] = Value::String(STANDARD.encode(b"{}")),
        _ => unreachable!(),
    }

    assert_eq!(
        super::build_provenance(
            &signed_field(attestation),
            SIGNED_SHA256,
            SIGNED_FILENAME,
            Some(&verification_context(
                SIGSTORE_PRODUCTION_TRUSTED_ROOT,
                SIGNED_IDENTITY,
                BTreeMap::new(),
            )),
        )
        .unwrap_err(),
        AttestationError::VerificationFailed(0)
    );
}

#[test]
fn test_mixed_attestations_reject_the_complete_upload() {
    let valid = signed_attestation();
    let mut invalid = valid.clone();
    invalid["envelope"]["signature"] = Value::String("AA==".to_owned());
    let context = verification_context(SIGSTORE_PRODUCTION_TRUSTED_ROOT, SIGNED_IDENTITY, BTreeMap::new());

    for attestations in [[valid.clone(), invalid.clone()], [invalid.clone(), valid]] {
        assert!(matches!(
            super::build_provenance(
                &serde_json::to_string(&attestations).unwrap(),
                SIGNED_SHA256,
                SIGNED_FILENAME,
                Some(&context),
            ),
            Err(AttestationError::VerificationFailed(_))
        ));
    }
}

const SHA: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";

fn statement(name: &str, sha: &str) -> String {
    encode_statement(&json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{"name": name, "digest": {"sha256": sha}}],
        "predicateType": "https://docs.pypi.org/attestations/publish/v1",
        "predicate": {},
    }))
}

fn encode_statement(statement: &Value) -> String {
    STANDARD.encode(statement.to_string())
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
    assert_eq!(
        bundle["publisher"],
        json!({"kind": "test", "claims": {"identity": "test"}})
    );
    assert_eq!(bundle["attestations"].as_array().unwrap().len(), 1);
    assert_eq!(bundle["attestations"][0]["version"], 1);
}

fn publisher(kind: &str, identity: &str) -> Publisher {
    Publisher {
        kind: kind.to_owned(),
        claims: BTreeMap::from([("identity".to_owned(), identity.to_owned())]),
    }
}

#[test]
fn test_provenance_groups_attestations_from_the_same_publisher() {
    let publisher = publisher("issuer", "identity");
    let first = attestation("first", SHA);
    let second = attestation("second", SHA);
    let document: Value = serde_json::from_slice(&provenance_document_from_attestations([
        (publisher.clone(), first.clone()),
        (publisher.clone(), second.clone()),
    ]))
    .unwrap();

    assert_eq!(
        document["attestation_bundles"],
        json!([{"publisher": publisher, "attestations": [first, second]}])
    );
}

#[rstest]
#[case::kind(publisher("issuer-a", "identity"), publisher("issuer-b", "identity"))]
#[case::claims(publisher("issuer", "identity-a"), publisher("issuer", "identity-b"))]
fn test_provenance_separates_distinct_publishers(#[case] first: Publisher, #[case] second: Publisher) {
    let first_attestation = attestation("first", SHA);
    let second_attestation = attestation("second", SHA);
    let document: Value = serde_json::from_slice(&provenance_document_from_attestations([
        (second.clone(), second_attestation.clone()),
        (first.clone(), first_attestation.clone()),
    ]))
    .unwrap();

    assert_eq!(
        document["attestation_bundles"],
        json!([
            {"publisher": first, "attestations": [first_attestation]},
            {"publisher": second, "attestations": [second_attestation]},
        ])
    );
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
    empty["envelope"]["statement"] = json!(encode_statement(&json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [],
        "predicateType": "https://docs.pypi.org/attestations/publish/v1",
    })));
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

#[rstest::rstest]
#[case::short(63, 'a')]
#[case::long(65, 'a')]
#[case::non_hex(64, 'g')]
fn test_build_provenance_rejects_an_invalid_subject_digest(#[case] length: usize, #[case] character: char) {
    let digest = character.to_string().repeat(length);
    let raw = field(&[attestation(FILENAME, &digest)]);

    assert_eq!(
        build_provenance(&raw, &digest, FILENAME).unwrap_err(),
        AttestationError::SubjectDigestMismatch(0)
    );
    assert_eq!(
        summarize_provenance(
            &provenance_document(&[attestation(FILENAME, &digest)]),
            &digest,
            FILENAME
        )
        .unwrap()[0]
            .subject,
        SubjectMatch::Mismatched
    );
    assert!(
        stored_predicate_types(
            &provenance_document(&[attestation(FILENAME, &digest)]),
            &digest,
            FILENAME
        )
        .is_empty()
    );
}

#[test]
fn test_build_provenance_and_stored_provenance_match_mixed_case_digests() {
    let digest = "aB".repeat(32);
    let document = build_provenance(
        &field(&[attestation(FILENAME, &digest)]),
        &digest.to_ascii_uppercase(),
        FILENAME,
    )
    .unwrap()
    .document;

    assert_eq!(
        summarize_provenance(&document, &digest.to_ascii_uppercase(), FILENAME).unwrap()[0].subject,
        SubjectMatch::Matched
    );
    assert_eq!(
        stored_predicate_types(&document, &digest.to_ascii_uppercase(), FILENAME),
        BTreeSet::from(["https://docs.pypi.org/attestations/publish/v1".to_owned()])
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
fn test_build_provenance_rejects_an_invalid_subject_filename() {
    let raw = field(&[attestation("peryxpkg-1.0.exe", SHA)]);

    assert_eq!(
        build_provenance(&raw, SHA, FILENAME).unwrap_err(),
        AttestationError::SubjectNameMismatch {
            index: 0,
            expected: FILENAME.to_owned(),
            actual: "peryxpkg-1.0.exe".to_owned(),
        }
    );
}

#[test]
fn test_build_provenance_rejects_a_subject_without_a_name() {
    let mut anonymous = attestation(FILENAME, SHA);
    anonymous["envelope"]["statement"] = json!(encode_statement(&json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{"digest": {"sha256": SHA}}],
        "predicateType": "https://docs.pypi.org/attestations/publish/v1",
    })));
    let raw = field(&[anonymous]);

    assert_eq!(
        build_provenance(&raw, SHA, FILENAME).unwrap_err(),
        AttestationError::MalformedStatement(0)
    );
}

#[rstest::rstest]
#[case::statement(json!([
    "https://in-toto.io/Statement/v1",
    [{"name": FILENAME, "digest": {"sha256": SHA}}],
    "https://docs.pypi.org/attestations/publish/v1",
]))]
#[case::subject(json!({
    "_type": "https://in-toto.io/Statement/v1",
    "subject": [[FILENAME, {"sha256": SHA}]],
    "predicateType": "https://docs.pypi.org/attestations/publish/v1",
}))]
fn test_build_provenance_rejects_positional_statements(#[case] statement: Value) {
    let mut attestation = attestation(FILENAME, SHA);
    attestation["envelope"]["statement"] = json!(encode_statement(&statement));

    assert_eq!(
        build_provenance(&field(&[attestation]), SHA, FILENAME).unwrap_err(),
        AttestationError::MalformedStatement(0)
    );
}

#[test]
fn test_build_provenance_rejects_extra_subjects() {
    let mut extra = attestation(FILENAME, SHA);
    extra["envelope"]["statement"] = json!(encode_statement(&json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [
            {"name": FILENAME, "digest": {"sha256": SHA}},
            {"name": FILENAME, "digest": {"sha256": SHA}},
        ],
        "predicateType": "https://docs.pypi.org/attestations/publish/v1",
    })));

    assert_eq!(
        build_provenance(&field(&[extra]), SHA, FILENAME).unwrap_err(),
        AttestationError::MultipleSubjects { index: 0, count: 2 }
    );
}

#[rstest::rstest]
#[case::missing_type(json!({
    "subject": [{"name": FILENAME, "digest": {"sha256": SHA}}],
    "predicateType": "https://docs.pypi.org/attestations/publish/v1",
}))]
#[case::wrong_type(json!({
    "_type": "https://in-toto.io/Statement/v0",
    "subject": [{"name": FILENAME, "digest": {"sha256": SHA}}],
    "predicateType": "https://docs.pypi.org/attestations/publish/v1",
}))]
#[case::missing_predicate_type(json!({
    "_type": "https://in-toto.io/Statement/v1",
    "subject": [{"name": FILENAME, "digest": {"sha256": SHA}}],
}))]
fn test_build_provenance_requires_in_toto_v1_fields(#[case] statement: Value) {
    let mut attestation = attestation(FILENAME, SHA);
    attestation["envelope"]["statement"] = json!(encode_statement(&statement));

    assert_eq!(
        build_provenance(&field(&[attestation]), SHA, FILENAME).unwrap_err(),
        AttestationError::MalformedStatement(0)
    );
}

#[test]
fn test_build_provenance_accepts_an_omitted_predicate() {
    let mut attestation = attestation(FILENAME, SHA);
    attestation["envelope"]["statement"] = json!(encode_statement(&json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{"name": FILENAME, "digest": {"sha256": SHA}}],
        "predicateType": "https://docs.pypi.org/attestations/publish/v1",
    })));

    assert!(build_provenance(&field(&[attestation]), SHA, FILENAME).is_ok());
}

#[rstest::rstest]
#[case::wheel("peryx.pkg-1.0-py3-none-any.whl", "peryx_pkg-1.0.0-py3-none-any.whl")]
#[case::sdist("peryx.pkg-1.0.tar.gz", "peryx-pkg-1.0.0.tar.gz")]
fn test_build_provenance_accepts_a_normalized_filename(#[case] filename: &str, #[case] subject: &str) {
    let raw = field(&[attestation(subject, SHA)]);

    assert!(build_provenance(&raw, SHA, filename).is_ok());
}

#[rstest::rstest]
#[case::build_number("peryxpkg-1.0-1-py3-none-any.whl", "peryxpkg-1.0-01-py3-none-any.whl")]
#[case::compatibility_tags("peryxpkg-1.0-py2.py3-none-any.whl", "peryxpkg-1.0-PY3.PY2-NONE-ANY.any.whl")]
fn test_build_provenance_accepts_an_equivalent_wheel_identity(#[case] filename: &str, #[case] subject: &str) {
    assert!(build_provenance(&field(&[attestation(subject, SHA)]), SHA, filename).is_ok());
}

#[test]
fn test_build_provenance_accepts_many_repeated_compatibility_tags() {
    let python = vec!["py3"; 3_000].join(".");
    let abi = vec!["none"; 3_000].join(".");
    let platform = vec!["any"; 3_000].join(".");
    let filename = format!("peryxpkg-1.0-{python}-{abi}-{platform}.whl");

    assert!(build_provenance(&field(&[attestation(&filename, SHA)]), SHA, &filename).is_ok());
}

#[rstest::rstest]
#[case::missing_platform("peryxpkg-1.0-py3-none.whl")]
#[case::extra_component("peryxpkg-1.0-build-py3-none-any-extra.whl")]
fn test_wheel_tags_reject_invalid_shapes(#[case] filename: &str) {
    assert_eq!(wheel_tags(filename), None);
}

#[rstest::rstest]
#[case::absent_build("peryxpkg-1.0-py3-none-any.whl", "peryxpkg-1.0-0-py3-none-any.whl")]
#[case::build_number("peryxpkg-1.0-1-py3-none-any.whl", "peryxpkg-1.0-2-py3-none-any.whl")]
#[case::build_suffix("peryxpkg-1.0-1A-py3-none-any.whl", "peryxpkg-1.0-1a-py3-none-any.whl")]
#[case::python_tag_set("peryxpkg-1.0-py2.py3-none-any.whl", "peryxpkg-1.0-py2.py4-none-any.whl")]
#[case::abi_tag("peryxpkg-1.0-py3-none-any.whl", "peryxpkg-1.0-py3-cp311-any.whl")]
#[case::platform_tag("peryxpkg-1.0-py3-none-any.whl", "peryxpkg-1.0-py3-none-win_amd64.whl")]
fn test_build_provenance_rejects_a_different_wheel_identity(#[case] filename: &str, #[case] subject: &str) {
    let raw = field(&[attestation(subject, SHA)]);

    assert_eq!(
        build_provenance(&raw, SHA, filename).unwrap_err(),
        AttestationError::SubjectNameMismatch {
            index: 0,
            expected: filename.to_owned(),
            actual: subject.to_owned(),
        }
    );
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
fn test_summarize_provenance_reads_a_normalized_filename() {
    let filename = "peryx.pkg-1.0-py3-none-any.whl";
    let document = provenance_document(&[attestation("peryx_pkg-1.0.0-py3-none-any.whl", SHA)]);

    let summaries = summarize_provenance(&document, SHA, filename).unwrap();

    assert_eq!(summaries[0].subject, SubjectMatch::Matched);
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

#[rstest]
#[case::missing(json!({"attestations": [attestation(FILENAME, SHA)]}))]
#[case::null(json!({"publisher": null, "attestations": [attestation(FILENAME, SHA)]}))]
#[case::missing_identity(json!({
    "publisher": {"kind": "issuer", "claims": {}},
    "attestations": [attestation(FILENAME, SHA)],
}))]
fn test_stored_provenance_rejects_an_unverified_publisher(#[case] bundle: Value) {
    let document = serde_json::to_vec(&json!({"version": 1, "attestation_bundles": [bundle]})).unwrap();

    assert_eq!(summarize_provenance(&document, SHA, FILENAME), None);
    assert!(stored_predicate_types(&document, SHA, FILENAME).is_empty());
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
            AttestationError::MultipleSubjects { index: 0, count: 2 },
            "names 2 subjects",
        ),
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
