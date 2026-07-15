//! Query parsing and matching, and the errors a bad query earns.

use super::support::*;

#[tokio::test]
async fn test_search_handles_empty_queries_and_fallback_params() {
    let h = harness().await;

    let (status, _headers, body) = get(&h.state, "/+search", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "query": "",
            "type": "all",
            "page": 1,
            "page_size": 25,
            "total": 0,
            "results": [],
        })
    );

    let (status, _headers, body) = get(
        &h.state,
        "/+search?q=re:&page=0&page_size=7&ignored=1",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["page"], 1);
    assert_eq!(value["page_size"], 25);
}
#[tokio::test]
async fn test_search_reports_invalid_type_filters() {
    let h = harness().await;
    for uri in ["/+search?type=blocked", "/hosted/+search?type=blocked"] {
        let (status, _headers, body) = get(&h.state, uri, Some("application/json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("invalid package source type"));
    }
}
#[tokio::test]
async fn test_search_reports_invalid_regex() {
    let h = harness().await;
    let (status, _headers, body) = get(&h.state, "/+search?q=re:(broken&page_size=25", Some("application/json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("RegexQueryError"));
}
#[tokio::test]
async fn test_search_reports_cached_detail_parse_errors() {
    let h = harness().await;
    h.state
        .meta
        .put_index(
            "pypi/broken",
            &cached_index("{\"meta\":{\"api-version\":\"1.1\"},\"files\":"),
        )
        .unwrap();
    h.state.bump_search_epoch();

    let (status, _headers, body) = get(&h.state, "/+search?q=broken&page_size=25", Some("application/json")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("EOF while parsing"));
}
#[tokio::test]
async fn test_search_matches_single_character_literal_queries() {
    let h = harness().await;
    put_uploaded_package(&h.state, "Peryx.Core", "peryx-core", "literal dot package");

    let (status, _headers, body) = get(&h.state, "/hosted/+search?q=.&page_size=25", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 1);
}
