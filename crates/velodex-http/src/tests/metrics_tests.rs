use crate::metrics::{Event, Metrics};
use axum::http::StatusCode;
use velodex_storage::blob::Digest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::http_tests::{get, harness};

fn settle(metrics: &Metrics, done: impl Fn(&Metrics) -> bool) {
    // The aggregator runs on its own thread; poll until the last event lands.
    for _ in 0..500 {
        if done(metrics) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("metrics aggregator never settled");
}

#[test]
fn test_events_aggregate_by_index_project_and_file() {
    let metrics = Metrics::start();
    metrics.record(Event::Page {
        route: "root/pypi".into(),
        project: "pandas".into(),
    });
    metrics.record(Event::Download {
        route: "root/pypi".into(),
        project: "pandas".into(),
        filename: "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl".into(),
        bytes: 100,
    });
    metrics.record(Event::Download {
        route: "root/pypi".into(),
        project: "pandas".into(),
        filename: "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl".into(),
        bytes: 50,
    });
    metrics.record(Event::Metadata {
        route: "root/pypi".into(),
        project: "pandas".into(),
        filename: "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl.metadata".into(),
    });
    metrics.record(Event::Upload {
        route: "root/pypi".into(),
        project: "velodexpkg".into(),
    });
    settle(&metrics, |m| {
        m.index_totals().get("root/pypi").is_some_and(|t| t.uploads == 1)
    });

    let totals = metrics.index_totals();
    let index = &totals["root/pypi"];
    assert_eq!(index.pages, 1);
    assert_eq!(index.downloads, 2);
    assert_eq!(index.bytes, 150);
    assert_eq!(index.metadata, 1);
    assert_eq!(index.uploads, 1);

    let projects = metrics.drill(Some("root/pypi"), None);
    assert_eq!(projects["projects"]["pandas"]["downloads"], 2);
    assert_eq!(projects["projects"]["velodexpkg"]["uploads"], 1);

    let files = metrics.drill(Some("root/pypi"), Some("pandas"));
    let file = &files["files"]["pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl"];
    assert_eq!(file["downloads"], 2);
    assert_eq!(file["bytes"], 150);
}

#[test]
fn test_drill_unknown_levels_are_empty() {
    let metrics = Metrics::start();
    metrics.record(Event::Page {
        route: "pypi".into(),
        project: "a".into(),
    });
    settle(&metrics, |m| !m.index_totals().is_empty());
    assert_eq!(metrics.drill(Some("ghost"), None), serde_json::json!({}));
    assert_eq!(metrics.drill(Some("pypi"), Some("ghost")), serde_json::json!({}));
    let top = metrics.drill(None, None);
    assert!(top["pypi"]["pages"].as_u64().unwrap() >= 1);
}

#[test]
fn test_operational_events_aggregate() {
    let metrics = Metrics::start();
    metrics.record(Event::Refresh {
        route: "pypi".into(),
        project: "flask".into(),
        changed: true,
    });
    metrics.record(Event::Refresh {
        route: "pypi".into(),
        project: "flask".into(),
        changed: false,
    });
    metrics.record(Event::StaleServed {
        route: "pypi".into(),
        project: "flask".into(),
    });
    metrics.record(Event::UpstreamError {
        route: "pypi".into(),
        project: "flask".into(),
    });
    metrics.record(Event::BlobRejected {
        route: "pypi".into(),
        project: "flask".into(),
    });
    settle(&metrics, |m| {
        m.index_totals().get("pypi").is_some_and(|t| t.rejected == 1)
    });

    let totals = metrics.index_totals();
    let index = &totals["pypi"];
    assert_eq!(index.refreshes, 2);
    assert_eq!(index.changed, 1);
    assert_eq!(index.stale_served, 1);
    assert_eq!(index.upstream_errors, 1);
    assert_eq!(index.rejected, 1);

    let projects = metrics.drill(Some("pypi"), None);
    assert_eq!(projects["projects"]["flask"]["refreshes"], 2);
    assert_eq!(projects["projects"]["flask"]["rejected"], 1);
}

#[tokio::test]
async fn test_router_paths_feed_stats_and_prometheus_metrics() {
    let harness = harness().await;
    let wheel = b"wheelcontent";
    let metadata = b"Metadata-Version: 2.1\nName: flask\n";
    let wheel_digest = Digest::of(wheel);
    let metadata_digest = Digest::of(metadata);
    let filename = "flask-1.0-py3-none-any.whl";
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"{filename}\",\"url\":\"{}/files/flask.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"core-metadata\":{{\"sha256\":\"{}\"}}}}]}}",
        harness.server.uri(),
        wheel_digest.as_str(),
        metadata_digest.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&harness.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata.to_vec()))
        .mount(&harness.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel.to_vec()))
        .mount(&harness.server)
        .await;

    let (page_status, ..) = get(&harness.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(page_status, StatusCode::OK);
    let metadata_uri = format!("/pypi/files/{}/{filename}.metadata", wheel_digest.as_str());
    let (metadata_status, ..) = get(&harness.state, &metadata_uri, None).await;
    assert_eq!(metadata_status, StatusCode::OK);
    let file_uri = format!("/pypi/files/{}/{filename}", wheel_digest.as_str());
    let (file_status, ..) = get(&harness.state, &file_uri, None).await;
    assert_eq!(file_status, StatusCode::OK);
    settle(&harness.state.metrics, |metrics| {
        metrics.index_totals().get("pypi").is_some_and(|totals| {
            totals.pages == 1 && totals.metadata == 1 && totals.downloads == 1 && totals.bytes == wheel.len() as u64
        })
    });

    let (stats_status, _, stats_body) = get(&harness.state, "/+stats?index=pypi&project=flask", None).await;
    let stats_json: serde_json::Value = serde_json::from_str(&stats_body).unwrap();
    assert_eq!(stats_status, StatusCode::OK);
    assert_eq!(
        stats_json,
        serde_json::json!({
            "pages": 1,
            "downloads": 1,
            "metadata": 1,
            "uploads": 0,
            "bytes": wheel.len() as u64,
            "refreshes": 0,
            "changed": 0,
            "stale_served": 0,
            "upstream_errors": 0,
            "rejected": 0,
            "files": {
                filename: {"downloads": 1, "metadata": 0, "bytes": wheel.len() as u64},
                format!("{filename}.metadata"): {"downloads": 0, "metadata": 1, "bytes": 0}
            }
        })
    );

    let (metrics_status, _, metrics_body) = get(&harness.state, "/metrics", None).await;
    assert_eq!(metrics_status, StatusCode::OK);
    for line in [
        "velodex_index_pages_total{index=\"pypi\"} 1",
        "velodex_index_downloads_total{index=\"pypi\"} 1",
        "velodex_index_download_bytes_total{index=\"pypi\"} 12",
        "velodex_index_metadata_total{index=\"pypi\"} 1",
    ] {
        assert!(metrics_body.contains(line), "{line} missing from:\n{metrics_body}");
    }
}
