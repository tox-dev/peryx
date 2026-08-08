//! Explaining virtual-repository shadowing over mixed hosted and cached members.

use peryx_core::{ShadowReason, ShadowSource};
use peryx_driver::shadow::{ShadowQuery, ShadowQueryError};
use peryx_policy::FallbackMode;

use super::support::*;

const HOSTED_FILE: &str = "acme_pkg-1.0-py3-none-any.whl";
const CACHED_FILE: &str = "acme_pkg-2.0-py3-none-any.whl";
const UPSTREAM_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn overlay_harness(mode: FallbackMode) -> Harness {
    let overlay = policy(|_, pypi| pypi.fallback_mode = mode);
    harness_with_policies(true, true, Policy::default(), Policy::default(), overlay).await
}

/// Serve an upstream page offering the contested `1.0` filename (a different digest than the hosted
/// upload) and a distinct `2.0` filename only the cache carries.
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

/// Populate the cache by fetching the upstream page through the cached member's own route.
async fn warm_cache(harness: &Harness) {
    let (status, _, _) = get(&harness.state, "/pypi/simple/acme-pkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
}

fn query() -> ShadowQuery {
    ShadowQuery::new("root/pypi".to_owned(), "acme-pkg".to_owned())
}

type Row<'a> = (&'a str, &'a str, bool, Option<ShadowReason>);

fn rows(page: &peryx_driver::shadow::ShadowPage) -> Vec<Row<'_>> {
    page.candidates
        .iter()
        .map(|candidate| {
            (
                candidate.member.as_str(),
                candidate.filename.as_str(),
                candidate.selected,
                candidate.reason,
            )
        })
        .collect()
}

#[tokio::test]
async fn test_fallback_selects_hosted_and_shadows_the_cached_duplicate_by_precedence() {
    let harness = overlay_harness(FallbackMode::Fallback).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");
    warm_cache(&harness).await;

    let page = harness.state.query_shadowed(&query()).unwrap();

    assert_eq!(
        rows(&page),
        vec![
            ("hosted", HOSTED_FILE, true, None),
            ("pypi", HOSTED_FILE, false, Some(ShadowReason::Precedence)),
            ("pypi", CACHED_FILE, true, None),
        ],
        "hosted wins the contested filename; the cache still supplies its distinct file"
    );
    let selected = &page.candidates[0];
    assert_eq!(selected.source, ShadowSource::Hosted);
    assert_eq!(selected.repository, "root/pypi");
    assert_eq!(selected.project, "acme-pkg");
    assert!(
        selected
            .digest
            .as_deref()
            .is_some_and(|digest| digest.starts_with("sha256:")),
        "the winning candidate carries its digest"
    );
    assert_eq!(page.candidates[2].source, ShadowSource::Cached);
    assert_eq!(page.next_cursor, None);
}

#[tokio::test]
async fn test_private_first_shadows_every_cached_candidate_when_hosted_is_present() {
    let harness = overlay_harness(FallbackMode::PrivateFirst).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");
    warm_cache(&harness).await;

    let page = harness.state.query_shadowed(&query()).unwrap();

    assert_eq!(
        rows(&page),
        vec![
            ("hosted", HOSTED_FILE, true, None),
            ("pypi", HOSTED_FILE, false, Some(ShadowReason::Fallback)),
            ("pypi", CACHED_FILE, false, Some(ShadowReason::Fallback)),
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

    let page = harness.state.query_shadowed(&query()).unwrap();

    let cached: Vec<Row<'_>> = rows(&page).into_iter().filter(|row| row.0 == "pypi").collect();
    assert!(
        cached.iter().all(|row| !row.2 && row.3 == Some(ShadowReason::Fallback)),
        "no-fallback never lets a cached member win: {cached:?}"
    );
    assert!(rows(&page).contains(&("hosted", HOSTED_FILE, true, None)));
}

#[tokio::test]
async fn test_an_unfetched_cache_contributes_no_candidates() {
    let harness = overlay_harness(FallbackMode::Fallback).await;
    mount_acme(&harness).await;
    put_local_project(&harness.state, "acme-pkg", HOSTED_FILE, b"hosted wheel", "1.0");

    let page = harness.state.query_shadowed(&query()).unwrap();

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

    let page = harness
        .state
        .query_shadowed(&ShadowQuery::new("hosted".to_owned(), "acme-pkg".to_owned()))
        .unwrap();

    assert!(page.candidates.is_empty());
}

#[tokio::test]
async fn test_a_corrupt_member_record_is_a_store_error() {
    let harness = overlay_harness(FallbackMode::Fallback).await;
    harness
        .state
        .meta
        .put_upload("hosted", "acme-pkg", HOSTED_FILE, b"not json")
        .unwrap();

    let error = harness.state.query_shadowed(&query()).unwrap_err();

    assert!(matches!(error, ShadowQueryError::Store(_)), "{error:?}");
}
