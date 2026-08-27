use base64::engine::general_purpose::STANDARD;
use peryx_identity::{GrantScope, Role};

use crate::policy::FallbackMode;

use super::support::*;

const HOSTED_FILE: &str = "acme_pkg-1.0-py3-none-any.whl";
const CACHED_FILE: &str = "acme_pkg-2.0-py3-none-any.whl";
const UPSTREAM_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn overlay_harness(mode: FallbackMode) -> Harness {
    let overlay = policy(|_, pypi| pypi.fallback_mode = mode);
    harness_with_policies(true, true, Policy::default(), Policy::default(), overlay).await
}

async fn mount_acme(harness: &Harness) {
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"acme-pkg\",\"versions\":[\"1.0\",\"2.0\"],\"files\":[\
         {{\"filename\":\"{HOSTED_FILE}\",\"url\":\"https://upstream.invalid/{HOSTED_FILE}\",\
         \"hashes\":{{\"sha256\":\"{UPSTREAM_DIGEST}\"}}}},\
         {{\"filename\":\"{CACHED_FILE}\",\"url\":\"https://upstream.invalid/{CACHED_FILE}\",\
         \"hashes\":{{\"sha256\":\"{UPSTREAM_DIGEST}\"}}}}]}}"
    );
    Mock::given(method("GET"))
        .and(path("/simple/acme-pkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&harness.server)
        .await;
}

async fn warm_cache(harness: &Harness) {
    let (status, _, _) = get(&harness.state, "/pypi/simple/acme-pkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
}

type Row<'a> = (&'a str, &'a str, bool, Option<&'a str>);

fn rows(page: &serde_json::Value) -> Vec<Row<'_>> {
    page["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| {
            (
                candidate["member"].as_str().unwrap(),
                candidate["filename"].as_str().unwrap(),
                candidate["selected"].as_bool().unwrap(),
                candidate["reason"].as_str(),
            )
        })
        .collect()
}

async fn candidates(harness: &Harness, repository: &str) -> (StatusCode, serde_json::Value) {
    let user = harness.state.serving.users.create("shadow-reader").unwrap();
    harness
        .state
        .serving
        .users
        .set_password(&user.id, "shadow-password")
        .await
        .unwrap();
    let name = harness
        .state
        .serving
        .indexes
        .iter()
        .find(|index| index.route == repository)
        .unwrap()
        .name
        .clone();
    harness
        .state
        .serving
        .authorization
        .grant(&user.id, Role::RepositoryReader, GrantScope::Repository { name })
        .unwrap();
    let authorization = format!("Basic {}", STANDARD.encode("shadow-reader:shadow-password"));
    let (status, body) = request_response(
        &harness.state,
        "GET",
        &format!("/+shadow/candidates?repository={repository}&project=acme-pkg"),
        Some(&authorization),
    )
    .await;
    (status, serde_json::from_str(&body).unwrap())
}

#[tokio::test]
async fn test_fallback_selects_hosted_and_shadows_the_cached_duplicate_by_precedence() {
    let harness = overlay_harness(FallbackMode::Fallback).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");
    warm_cache(&harness).await;

    let (status, page) = candidates(&harness, "root/pypi").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&page),
        vec![
            ("hosted", HOSTED_FILE, true, None),
            ("pypi", HOSTED_FILE, false, Some("precedence")),
            ("pypi", CACHED_FILE, true, None),
        ],
        "hosted wins the contested filename; the cache still supplies its distinct file"
    );
    let selected = &page["candidates"][0];
    assert_eq!(selected["source"], "hosted");
    assert!(
        selected["digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:")),
        "the winning candidate carries its digest"
    );
    assert_eq!(page["candidates"][2]["source"], "cached");
    assert_eq!(page["next_cursor"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_private_first_shadows_every_cached_candidate_when_hosted_is_present() {
    let harness = overlay_harness(FallbackMode::PrivateFirst).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");
    warm_cache(&harness).await;

    let (status, page) = candidates(&harness, "root/pypi").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&page),
        vec![
            ("hosted", HOSTED_FILE, true, None),
            ("pypi", HOSTED_FILE, false, Some("fallback")),
            ("pypi", CACHED_FILE, false, Some("fallback")),
        ],
        "private-first excludes both cached candidates in favor of the hosted member"
    );
}

#[tokio::test]
async fn test_no_fallback_excludes_cached_members() {
    let harness = overlay_harness(FallbackMode::NoFallback).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");
    warm_cache(&harness).await;

    let (status, page) = candidates(&harness, "root/pypi").await;

    assert_eq!(status, StatusCode::OK);
    let cached: Vec<Row<'_>> = rows(&page).into_iter().filter(|row| row.0 == "pypi").collect();
    assert!(
        cached.iter().all(|row| !row.2 && row.3 == Some("fallback")),
        "no-fallback never lets a cached member win: {cached:?}"
    );
    assert!(rows(&page).contains(&("hosted", HOSTED_FILE, true, None)));
}

#[tokio::test]
async fn test_an_unfetched_cache_contributes_no_candidates() {
    let harness = overlay_harness(FallbackMode::Fallback).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");

    let (status, page) = candidates(&harness, "root/pypi").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&page),
        vec![("hosted", HOSTED_FILE, true, None)],
        "with no stored cache page only the hosted candidate is known"
    );
}

#[tokio::test]
async fn test_a_non_virtual_repository_shadows_nothing() {
    let harness = overlay_harness(FallbackMode::Fallback).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");

    let (status, page) = candidates(&harness, "hosted").await;

    assert_eq!(status, StatusCode::OK);
    assert!(page["candidates"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_a_corrupt_member_record_is_a_store_error() {
    let harness = overlay_harness(FallbackMode::Fallback).await;
    harness
        .state
        .serving
        .meta
        .put_upload("hosted", "acme-pkg", HOSTED_FILE, b"not json")
        .unwrap();

    let (status, body) = candidates(&harness, "root/pypi").await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, serde_json::json!({"error": "shadow query failed"}));
}
