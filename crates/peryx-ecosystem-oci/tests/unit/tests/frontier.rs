//! A replica holds a hosted repository's mutable derived views (a tag resolution, tag list, or
//! referrers list) behind the readable frontier until its search view catches up to the serial that
//! changed them. A by-digest read is content-addressed and never held, and a cached or virtual index
//! reports no local journal serial, so neither is gated.

use axum::http::{Method, StatusCode};
use peryx_driver::state::SEARCH_VIEW;
use rstest::rstest;

use super::{app_with_journal, auth, oci_digest, oci_index, proxy, replica_router, send, send_body, writable_index};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

async fn push_manifest(app: &axum::Router, reference: &str, body: &[u8]) -> StatusCode {
    send_body(
        app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{reference}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        body.to_vec(),
    )
    .await
    .0
}

fn hosted_replica(
    state: &std::sync::Arc<peryx_driver::AppState>,
) -> (std::sync::Arc<peryx_driver::AppState>, axum::Router) {
    replica_router(state, vec![writable_index("store", "store", false, TOKEN)])
}

/// A tag resolution and a tag list are both mutable derived views of a hosted repository, so a replica
/// holds each behind the readable frontier and serves it, with its content, once the search view
/// catches up.
#[rstest]
#[case::tag("/v2/store/app/manifests/v1", r#""schemaVersion":2"#)]
#[case::tag_list("/v2/store/app/tags/list", r#""v1""#)]
#[tokio::test]
async fn test_replica_holds_a_hosted_mutable_read_until_the_search_view_catches_up(
    #[case] uri: &str,
    #[case] expected: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_journal(&dir, vec![writable_index("store", "store", false, TOKEN)], true);
    assert_eq!(push_manifest(&app, "v1", MANIFEST).await, StatusCode::CREATED);
    let published = state.meta.current_serial().unwrap();
    assert!(published > 0, "the push advanced the journal");

    let (_replica_state, replica) = hosted_replica(&state);
    let (held, ..) = send(&replica, Method::GET, uri).await;
    assert_eq!(held, StatusCode::NOT_FOUND);

    state.meta.set_view_frontier(SEARCH_VIEW, published).unwrap();
    let (served, _, body) = send(&replica, Method::GET, uri).await;
    assert_eq!(served, StatusCode::OK);
    assert!(std::str::from_utf8(&body).unwrap().contains(expected), "{body:?}");
}

#[tokio::test]
async fn test_replica_serves_a_hosted_tag_at_the_frontier() {
    // The gate is a strict `>`: a serial equal to the readable frontier is served, not held.
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_journal(&dir, vec![writable_index("store", "store", false, TOKEN)], true);
    assert_eq!(push_manifest(&app, "v1", MANIFEST).await, StatusCode::CREATED);
    let published = state.meta.current_serial().unwrap();
    state.meta.set_view_frontier(SEARCH_VIEW, published).unwrap();

    let (_replica_state, replica) = hosted_replica(&state);
    let (served, ..) = send(&replica, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(served, StatusCode::OK);
}

#[tokio::test]
async fn test_replica_holds_hosted_referrers_until_the_search_view_catches_up() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_journal(&dir, vec![writable_index("store", "store", false, TOKEN)], true);
    let subject = oci_digest(MANIFEST);
    assert_eq!(push_manifest(&app, "v1", MANIFEST).await, StatusCode::CREATED);
    let referrer = format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","artifactType":"application/vnd.example.sig","subject":{{"digest":"{subject}"}}}}"#,
    );
    let referrer_digest = oci_digest(referrer.as_bytes());
    assert_eq!(
        push_manifest(&app, &referrer_digest, referrer.as_bytes()).await,
        StatusCode::CREATED
    );
    let published = state.meta.current_serial().unwrap();

    let (_replica_state, replica) = hosted_replica(&state);
    let (held, _, body) = send(&replica, Method::GET, &format!("/v2/store/app/referrers/{subject}")).await;
    assert_eq!(held, StatusCode::OK);
    assert!(
        !std::str::from_utf8(&body).unwrap().contains(&referrer_digest),
        "held referrers leak: {body:?}"
    );

    state.meta.set_view_frontier(SEARCH_VIEW, published).unwrap();
    let (served, _, body) = send(&replica, Method::GET, &format!("/v2/store/app/referrers/{subject}")).await;
    assert_eq!(served, StatusCode::OK);
    assert!(
        std::str::from_utf8(&body).unwrap().contains(&referrer_digest),
        "{body:?}"
    );
}

#[tokio::test]
async fn test_replica_serves_a_hosted_manifest_by_digest_below_the_frontier() {
    // A by-digest read is content-addressed, the OCI analogue of PyPI's files/ route, so a lagging
    // search view never holds it.
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_journal(&dir, vec![writable_index("store", "store", false, TOKEN)], true);
    let digest = oci_digest(MANIFEST);
    assert_eq!(push_manifest(&app, "v1", MANIFEST).await, StatusCode::CREATED);

    let (_replica_state, replica) = hosted_replica(&state);
    let (status, _, body) = send(&replica, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, MANIFEST);
}

#[tokio::test]
async fn test_primary_serves_a_hosted_tag_regardless_of_the_search_view() {
    // The primary is never read-only, so a lagging search view never holds its reads.
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = app_with_journal(&dir, vec![writable_index("store", "store", false, TOKEN)], true);
    assert_eq!(push_manifest(&app, "v1", MANIFEST).await, StatusCode::CREATED);
    let (status, ..) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
}

/// A virtual index (`team`) that layers a hosted store surfaces the store's manifest, so a replica holds
/// the virtual tag read behind the readable frontier exactly as it holds the store's own route, then
/// serves it once the search view catches up.
#[tokio::test]
async fn test_replica_holds_a_virtual_read_of_a_hosted_member_until_readable() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_journal(&dir, vec![writable_index("store", "store", false, TOKEN)], true);
    assert_eq!(push_manifest(&app, "v1", MANIFEST).await, StatusCode::CREATED);
    let published = state.meta.current_serial().unwrap();
    assert!(published > 0, "the push advanced the journal");

    let (_replica_state, replica) = replica_router(
        &state,
        vec![
            writable_index("store", "store", false, TOKEN),
            oci_index(
                "team",
                "team",
                peryx_index::IndexKind::Virtual {
                    layers: vec![0],
                    upload: Some(0),
                },
            ),
        ],
    );
    let (held, ..) = send(&replica, Method::GET, "/v2/team/app/manifests/v1").await;
    assert_eq!(
        held,
        StatusCode::NOT_FOUND,
        "the virtual index leaked a member below the frontier"
    );

    state.meta.set_view_frontier(SEARCH_VIEW, published).unwrap();
    let (served, _, body) = send(&replica, Method::GET, "/v2/team/app/manifests/v1").await;
    assert_eq!(served, StatusCode::OK);
    assert!(
        std::str::from_utf8(&body).unwrap().contains(r#""schemaVersion":2"#),
        "{body:?}"
    );
}

#[tokio::test]
async fn test_replica_does_not_gate_a_cached_index() {
    // A cached index reports no local journal serial the readable frontier governs, so the read-only
    // gate is transparent to it: the replica answers a tag list exactly as the primary does.
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (primary_status, ..) = send(&app, Method::GET, "/v2/hub/app/tags/list").await;

    let (_replica_state, replica) = replica_router(
        &state,
        vec![super::oci_index(
            "hub",
            "hub",
            peryx_index::IndexKind::Cached {
                client: peryx_upstream::UpstreamClient::new("http://127.0.0.1:1/").unwrap(),
                offline: false,
            },
        )],
    );
    let (replica_status, ..) = send(&replica, Method::GET, "/v2/hub/app/tags/list").await;
    assert_eq!(replica_status, primary_status);
    assert_ne!(replica_status, StatusCode::NOT_FOUND);
}
