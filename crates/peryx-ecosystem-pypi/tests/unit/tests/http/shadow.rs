use base64::engine::general_purpose::STANDARD;
use peryx_identity::{GrantScope, Role};

use crate::policy::FallbackMode;

use super::support::*;

const HOSTED_FILE: &str = "acme_pkg-1.0-py3-none-any.whl";
const CACHED_FILE: &str = "acme_pkg-2.0-py3-none-any.whl";
const PROTECTED: &str = "mycorp-tool";
const PROTECTED_HOSTED_FILE: &str = "mycorp_tool-1.0-py3-none-any.whl";
const PROTECTED_CACHED_FILE: &str = "mycorp_tool-2.0-py3-none-any.whl";
const UPSTREAM_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn overlay_harness(mode: FallbackMode) -> Harness {
    let overlay = policy(|_, pypi| pypi.fallback_mode = mode);
    harness_with_policies(true, true, Policy::default(), Policy::default(), overlay).await
}

async fn protected_harness(mode: FallbackMode) -> Harness {
    let overlay = policy(|_, pypi| {
        pypi.fallback_mode = mode;
        pypi.protected_names = vec!["mycorp-*".to_owned()];
    });
    harness_with_policies(true, true, Policy::default(), Policy::default(), overlay).await
}

async fn mount_upstream(harness: &Harness, project: &str, files: [(&str, &str); 2]) {
    let versions = files
        .iter()
        .map(|(_, version)| format!("\"{version}\""))
        .collect::<Vec<_>>()
        .join(",");
    let entries = files
        .iter()
        .map(|(filename, _)| {
            format!(
                "{{\"filename\":\"{filename}\",\"size\":11,\"url\":\"https://upstream.invalid/{filename}\",\
                 \"hashes\":{{\"sha256\":\"{UPSTREAM_DIGEST}\"}}}}"
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"{project}\",\
         \"versions\":[{versions}],\"files\":[{entries}]}}"
    );
    Mock::given(method("GET"))
        .and(path(format!("/simple/{project}/")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&harness.server)
        .await;
}

async fn mount_acme(harness: &Harness) {
    mount_upstream(harness, "acme-pkg", [(HOSTED_FILE, "1.0"), (CACHED_FILE, "2.0")]).await;
}

async fn mount_protected(harness: &Harness) {
    mount_upstream(
        harness,
        PROTECTED,
        [(PROTECTED_HOSTED_FILE, "1.0"), (PROTECTED_CACHED_FILE, "2.0")],
    )
    .await;
}

async fn warm_cache(harness: &Harness, project: &str) {
    let (status, _, _) = get(
        &harness.state,
        &format!("/pypi/simple/{project}/"),
        Some("application/json"),
    )
    .await;
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

async fn candidates(harness: &Harness, repository: &str, project: &str) -> (StatusCode, serde_json::Value) {
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
        &format!("/+shadow/candidates?repository={repository}&project={project}"),
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
    warm_cache(&harness, "acme-pkg").await;

    let (status, page) = candidates(&harness, "root/pypi", "acme-pkg").await;

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
    warm_cache(&harness, "acme-pkg").await;

    let (status, page) = candidates(&harness, "root/pypi", "acme-pkg").await;

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
async fn test_a_cache_below_a_nested_member_is_recorded_as_a_cached_candidate() {
    let harness = nested_harness(policy(|_, pypi| pypi.fallback_mode = FallbackMode::PrivateFirst)).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");
    warm_cache(&harness, "acme-pkg").await;

    let (status, page) = candidates(&harness, "root/pypi", "acme-pkg").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&page),
        vec![
            ("hosted", HOSTED_FILE, true, None),
            ("pypi", HOSTED_FILE, false, Some("fallback")),
            ("pypi", CACHED_FILE, false, Some("fallback")),
        ],
        "the nested container is not a candidate; the cache it reaches is"
    );
}

#[tokio::test]
async fn test_no_fallback_excludes_cached_members() {
    let harness = overlay_harness(FallbackMode::NoFallback).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");
    warm_cache(&harness, "acme-pkg").await;

    let (status, page) = candidates(&harness, "root/pypi", "acme-pkg").await;

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

    let (status, page) = candidates(&harness, "root/pypi", "acme-pkg").await;

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

    let (status, page) = candidates(&harness, "hosted", "acme-pkg").await;

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

    let (status, body) = candidates(&harness, "root/pypi", "acme-pkg").await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, serde_json::json!({"error": "shadow query failed"}));
}

#[tokio::test]
async fn test_a_protected_name_selects_no_cached_candidate() {
    let harness = protected_harness(FallbackMode::Fallback).await;
    mount_protected(&harness).await;
    warm_cache(&harness, PROTECTED).await;

    let (status, page) = candidates(&harness, "root/pypi", PROTECTED).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&page),
        vec![
            ("pypi", PROTECTED_HOSTED_FILE, false, Some("protected-name")),
            ("pypi", PROTECTED_CACHED_FILE, false, Some("protected-name")),
        ],
        "a protected name reaches no cached candidate, so the replay selects none"
    );
}

#[tokio::test]
async fn test_a_protected_name_denies_the_route_the_replay_reports() {
    let harness = protected_harness(FallbackMode::Fallback).await;
    mount_protected(&harness).await;
    warm_cache(&harness, PROTECTED).await;

    let (status, _, body) = get(
        &harness.state,
        &format!("/root/pypi/simple/{PROTECTED}/"),
        Some("application/json"),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["rule"],
        "protected-name",
        "the route's own denial names the rule the replay has to report"
    );
}

#[rstest::rstest]
#[case::fallback(FallbackMode::Fallback)]
#[case::private_first(FallbackMode::PrivateFirst)]
#[case::no_fallback(FallbackMode::NoFallback)]
#[tokio::test]
async fn test_a_protected_name_keeps_its_hosted_candidate_in_every_mode(#[case] mode: FallbackMode) {
    let harness = protected_harness(mode).await;
    mount_protected(&harness).await;
    put_local_project(&harness.state, PROTECTED, PROTECTED_HOSTED_FILE, b"hosted wheel", "1.0");
    warm_cache(&harness, PROTECTED).await;

    let (status, page) = candidates(&harness, "root/pypi", PROTECTED).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&page),
        vec![
            ("hosted", PROTECTED_HOSTED_FILE, true, None),
            ("pypi", PROTECTED_HOSTED_FILE, false, Some("protected-name")),
            ("pypi", PROTECTED_CACHED_FILE, false, Some("protected-name")),
        ],
        "the protected-name rule outranks the fallback mode, so it names the exclusion"
    );
}
