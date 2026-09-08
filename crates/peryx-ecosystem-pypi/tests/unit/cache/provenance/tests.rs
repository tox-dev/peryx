#[test]
fn test_artifact_project_falls_back_for_a_legacy_distribution() {
    assert_eq!(super::artifact_project("Legacy_Name-1.0.egg"), "legacy-name");
}

fn attestation_response(declared: Option<usize>, body: Vec<u8>) -> reqwest::Response {
    let mut builder = axum::http::Response::builder()
        .status(200)
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    if let Some(declared) = declared {
        builder = builder.header(axum::http::header::CONTENT_LENGTH, declared.to_string());
    }
    reqwest::Response::from(builder.body(body).unwrap())
}

fn refused_for_size(outcome: &Result<super::FetchOutcome, crate::cache::CacheError>) -> bool {
    matches!(
        outcome,
        Err(crate::cache::CacheError::Upstream(
            peryx_upstream::UpstreamError::ResponseTooLarge { .. }
        ))
    )
}

/// The limit admits a bundle of exactly its size and refuses only what passes it, and it says so twice:
/// once from the length upstream declares, and again from the bytes that actually arrive, since a
/// declared length is a claim rather than a measurement.
#[tokio::test]
async fn test_a_declared_length_of_exactly_the_limit_is_not_refused_for_size() {
    let outcome =
        super::process_upstream_attestation_response(attestation_response(Some(super::MAX_PROVENANCE_BYTES), b"{}".to_vec()))
            .await;

    assert!(!refused_for_size(&outcome), "a bundle at the limit is within it");
}

#[tokio::test]
async fn test_a_declared_length_past_the_limit_is_refused_for_size() {
    let outcome = super::process_upstream_attestation_response(attestation_response(
        Some(super::MAX_PROVENANCE_BYTES + 1),
        b"{}".to_vec(),
    ))
    .await;

    assert!(refused_for_size(&outcome));
}

#[tokio::test]
async fn test_arriving_bytes_of_exactly_the_limit_are_not_refused_for_size() {
    let body = vec![b'x'; super::MAX_PROVENANCE_BYTES];

    let outcome = super::process_upstream_attestation_response(attestation_response(None, body)).await;

    assert!(!refused_for_size(&outcome), "bytes at the limit are within it");
}

#[tokio::test]
async fn test_arriving_bytes_past_the_limit_are_refused_for_size() {
    let body = vec![b'x'; super::MAX_PROVENANCE_BYTES + 1];

    let outcome = super::process_upstream_attestation_response(attestation_response(None, body)).await;

    assert!(refused_for_size(&outcome));
}
