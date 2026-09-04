use super::support::*;
use peryx_driver::serving::{CachePurgeDriver as _, PurgeReport};
use peryx_identity::IndexAcl;
use peryx_index::serving::flight_gate;

/// Bounds a handshake that resolves at once when the fence holds; nothing paces itself by this.
const HANDSHAKE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

fn cached_pypi(client: UpstreamClient) -> Vec<Index> {
    vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached { client, offline: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }]
}

fn stale_record(body: &str) -> CachedIndex {
    CachedIndex {
        fetched_at_unix: 0,
        fresh_secs: Some(1),
        ..fresh_record(body.as_bytes())
    }
}

fn project_page(project: &str, digest: &str) -> String {
    format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"{project}\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"{project}-1.0-py3-none-any.whl\",\"size\":11,\
         \"url\":\"https://files.example/{project}-1.0.whl\",\"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}"
    )
}

fn removed(index_pages: u64, project_records: u64, file_url_records: u64, metadata_records: u64) -> Vec<(String, u64)> {
    vec![
        ("index_pages".to_owned(), index_pages),
        ("project_records".to_owned(), project_records),
        ("file_url_records".to_owned(), file_url_records),
        ("metadata_records".to_owned(), metadata_records),
    ]
}

async fn cached_flask(h: &Harness) -> Digest {
    let digest = Digest::of(b"wheel-v1");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_json(digest.as_str(), &file_url)).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    digest
}

#[tokio::test]
async fn test_online_purge_removes_the_page_a_racing_refresh_published() {
    let dir = tempfile::tempdir().unwrap();
    let refreshed = Digest::of(b"wheel-v2");
    let mut upstream = stalled_response(200, project_page("flask", refreshed.as_str()).into_bytes());
    let state = custom_state(&dir, &upstream.upstream, cached_pypi);
    state
        .serving
        .meta
        .put_index(
            "pypi/flask",
            &stale_record(&project_page("flask", Digest::of(b"wheel-v1").as_str())),
        )
        .unwrap();
    let sweep = tokio::spawn({
        let state = state.clone();
        async move { cache::refresh_stale_pages(&state.serving).await }
    });
    upstream.wait_until_entered().await;
    let mut joins = state
        .serving
        .cache
        .inflight
        .subscribe("pypi/flask")
        .expect("the sweep owns the project flight");
    let purge = tokio::spawn({
        let state = state.clone();
        async move { cache::purge_served_project(&state.serving, "pypi", "Flask", true).await }
    });
    joins
        .next_join()
        .await
        .expect("the purge joins the flight it must wait on");

    upstream.release();
    let summary = sweep.await.unwrap().unwrap();
    let report = purge.await.unwrap().unwrap();

    assert_eq!((summary.checked, summary.changed), (1, 1));
    assert_eq!(
        report,
        PurgeReport {
            resource: "flask".to_owned(),
            categories: removed(1, 1, 1, 0),
        }
    );
    assert!(state.serving.meta.get_index("pypi/flask").unwrap().is_none());
    assert!(
        state
            .serving
            .meta
            .get_file_url("pypi", "flask", refreshed.as_str())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_stale_sweep_leaves_a_project_purged_while_it_queued() {
    let h = harness().await;
    cached_flask(&h).await;
    h.clock.fetch_add(61, Ordering::Relaxed);
    let gate = flight_gate(&h.state.serving.cache.inflight, "pypi/flask");
    let guard = gate.lock_owned().await;
    let mut joins = h
        .state
        .serving
        .cache
        .inflight
        .subscribe("pypi/flask")
        .expect("the purge owns the project flight");
    let sweep = tokio::spawn({
        let state = h.state.clone();
        async move { cache::refresh_stale_pages(&state.serving).await }
    });
    let joined = tokio::time::timeout(HANDSHAKE_DEADLINE, joins.next_join())
        .await
        .is_ok();

    assert!(
        joined,
        "a sweep that reaches upstream without joining the flight cannot be fenced"
    );

    let report = crate::admin::purge_project(&h.state.serving.meta, "pypi", "flask", true).unwrap();
    drop(guard);
    let summary = sweep.await.unwrap().unwrap();

    assert_eq!(report.categories, removed(1, 1, 1, 0));
    assert_eq!((summary.checked, summary.changed), (0, 0));
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_none());
}

#[tokio::test]
async fn test_online_purges_of_different_projects_do_not_wait_on_each_other() {
    let h = harness().await;
    cached_flask(&h).await;
    let requests = Digest::of(b"requests wheel");
    Mock::given(method("GET"))
        .and(path("/simple/requests/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            project_page("requests", requests.as_str()).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/requests/", Some("application/json")).await;
    let gate = flight_gate(&h.state.serving.cache.inflight, "pypi/flask");
    let guard = gate.lock_owned().await;

    let report = tokio::time::timeout(
        HANDSHAKE_DEADLINE,
        cache::purge_served_project(&h.state.serving, "pypi", "requests", true),
    )
    .await
    .expect("a purge of another project does not queue behind the held flight")
    .unwrap();

    assert_eq!(
        report,
        PurgeReport {
            resource: "requests".to_owned(),
            categories: removed(1, 1, 1, 0),
        }
    );
    assert!(h.state.serving.meta.get_index("pypi/requests").unwrap().is_none());
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_some());
    drop(guard);
}

#[tokio::test]
async fn test_online_purge_preview_counts_without_removing() {
    let h = harness().await;
    let digest = cached_flask(&h).await;

    let report = crate::serving::PypiServing
        .purge_served_resource(h.state.serving.clone(), "pypi", "Flask", false)
        .await
        .unwrap();

    assert_eq!(
        report,
        PurgeReport {
            resource: "flask".to_owned(),
            categories: removed(1, 1, 1, 0),
        }
    );
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_some());
    assert!(
        h.state
            .serving
            .meta
            .get_file_url("pypi", "flask", digest.as_str())
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn test_online_purge_of_an_undecodable_page_keeps_the_row() {
    let h = harness().await;
    cached_flask(&h).await;
    h.state
        .serving
        .meta
        .put_index("pypi/flask", &fresh_record(b"{ not a project page"))
        .unwrap();

    let error = cache::purge_served_project(&h.state.serving, "pypi", "flask", true)
        .await
        .unwrap_err();

    assert!(error.starts_with("read cached project pypi/flask: "));
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_some());
}
