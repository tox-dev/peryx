use axum::http::StatusCode;
use peryx_events::metrics::{Metrics, Observation};
use peryx_storage::blob::Digest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::http::{get, get_bytes_with_headers, harness};
use crate::catalog_job::{CatalogMetricOutcome, record_catalog_metrics};

fn settle(metrics: &Metrics, done: impl Fn(&Metrics) -> bool) {
    metrics.flush().unwrap();
    assert!(done(metrics), "metrics settled on an unexpected state");
}

#[test]
fn test_events_aggregate_by_index_project_and_file() {
    let metrics = Metrics::start();
    metrics.record(Observation::Page {
        repository: "root/pypi".into(),
        resource: "pandas".into(),
    });
    metrics.record(Observation::Read {
        repository: "root/pypi".into(),
        resource: "pandas".into(),
        artifact: "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl".into(),
        group: None,
        source: None,
        bytes: 100,
    });
    metrics.record(Observation::Read {
        repository: "root/pypi".into(),
        resource: "pandas".into(),
        artifact: "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl".into(),
        group: None,
        source: None,
        bytes: 50,
    });
    metrics.record(Observation::Ecosystem {
        repository: "root/pypi".into(),
        resource: "pandas".into(),
        artifact: Some("pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl.metadata".into()),
        family: "metadata",
    });
    metrics.record(Observation::Write {
        repository: "root/pypi".into(),
        resource: "peryxpkg".into(),
    });
    settle(&metrics, |m| {
        m.index_totals().get("root/pypi").is_some_and(|t| t.hosted.writes == 1)
    });

    let totals = metrics.index_totals();
    let index = &totals["root/pypi"];
    assert_eq!(index.base.pages, 1);
    assert_eq!(index.base.reads, 2);
    assert_eq!(index.base.bytes, 150);
    assert_eq!(index.ecosystem["metadata"], 1);
    assert_eq!(index.hosted.writes, 1);

    let projects = metrics.drill(Some("root/pypi"), None);
    assert_eq!(projects["resources"]["pandas"]["base"]["reads"], 2);
    assert_eq!(projects["resources"]["peryxpkg"]["hosted"]["writes"], 1);

    let files = metrics.drill(Some("root/pypi"), Some("pandas"));
    let file = &files["artifacts"]["pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl"];
    assert_eq!(file["reads"], 2);
    assert_eq!(file["bytes"], 150);
}

#[test]
fn test_drill_unknown_levels_are_empty() {
    let metrics = Metrics::start();
    metrics.record(Observation::Page {
        repository: "pypi".into(),
        resource: "a".into(),
    });
    settle(&metrics, |m| !m.index_totals().is_empty());
    assert_eq!(metrics.drill(Some("ghost"), None), serde_json::json!({}));
    assert_eq!(metrics.drill(Some("pypi"), Some("ghost")), serde_json::json!({}));
    let top = metrics.drill(None, None);
    assert!(top["pypi"]["base"]["pages"].as_u64().unwrap() >= 1);
}

#[test]
fn test_operational_events_aggregate() {
    let metrics = Metrics::start();
    metrics.record(Observation::Refresh {
        repository: "pypi".into(),
        resource: "flask".into(),
        changed: true,
    });
    metrics.record(Observation::Refresh {
        repository: "pypi".into(),
        resource: "flask".into(),
        changed: false,
    });
    metrics.record(Observation::StaleServed {
        repository: "pypi".into(),
        resource: "flask".into(),
    });
    metrics.record(Observation::UpstreamError {
        repository: "pypi".into(),
        resource: "flask".into(),
    });
    metrics.record(Observation::BlobRejected {
        repository: "pypi".into(),
        resource: "flask".into(),
    });
    for outcome in [
        CatalogMetricOutcome::Published { projects: 700_000 },
        CatalogMetricOutcome::NotModified { projects: 700_000 },
        CatalogMetricOutcome::Error,
    ] {
        record_catalog_metrics(&metrics, "pypi", outcome);
    }
    settle(&metrics, |m| {
        m.index_totals().get("pypi").is_some_and(|t| t.base.rejected == 1)
    });

    let totals = metrics.index_totals();
    let index = &totals["pypi"];
    assert_eq!(index.cached.refreshes, 2);
    assert_eq!(index.cached.changed, 1);
    assert_eq!(index.cached.stale_served, 1);
    assert_eq!(index.cached.upstream_errors, 1);
    assert_eq!(index.base.rejected, 1);
    assert_eq!(
        index.extensions,
        [
            ("pypi.catalog.errors", 1),
            ("pypi.catalog.not_modified", 1),
            ("pypi.catalog.projects", 700_000),
            ("pypi.catalog.published", 1),
            ("pypi.catalog.syncs", 3),
        ]
        .into()
    );

    let projects = metrics.drill(Some("pypi"), None);
    assert_eq!(projects["resources"]["flask"]["cached"]["refreshes"], 2);
    assert_eq!(projects["resources"]["flask"]["base"]["rejected"], 1);
    assert_eq!(projects["resources"].as_object().unwrap().len(), 1);
}

#[tokio::test]
async fn test_router_paths_feed_stats_and_prometheus_metrics() {
    let harness = placement_harness().await;
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
    settle(&harness.state.serving.metrics, |metrics| {
        metrics.index_totals().get("pypi").is_some_and(|totals| {
            totals.base.pages == 1
                && totals.ecosystem.get("metadata").copied() == Some(1)
                && totals.base.reads == 1
                && totals.base.bytes == wheel.len() as u64
        })
    });

    let authorization = crate::tests::administrator_header(&harness.state).await;
    let credentials = [(axum::http::header::AUTHORIZATION.as_str(), authorization.as_str())];
    let (stats_status, _, stats_bytes) =
        get_bytes_with_headers(&harness.state, "/+stats?repository=pypi&resource=flask", &credentials).await;
    let stats_json: serde_json::Value = serde_json::from_slice(&stats_bytes).unwrap();
    assert_eq!(stats_status, StatusCode::OK);
    assert_eq!(
        stats_json,
        serde_json::json!({
            "totals": {
                "base": {"pages": 1, "reads": 1, "bytes": wheel.len() as u64, "rejected": 0},
                "cached": {
                    "refreshes": 0,
                    "changed": 0,
                    "stale_served": 0,
                    "upstream_errors": 0,
                    "catalog_syncs": 0,
                    "catalog_published": 0,
                    "catalog_not_modified": 0,
                    "catalog_errors": 0,
                    "catalog_projects": 0
                },
                "hosted": {"writes": 0},
                "ecosystem": {"metadata": 1}
            },
            "artifacts": {
                filename: {"reads": 1, "bytes": wheel.len() as u64, "ecosystem": {}},
                format!("{filename}.metadata"): {"reads": 0, "bytes": 0, "ecosystem": {"metadata": 1}}
            }
        })
    );

    let (metrics_status, _, metrics_body) = get(&harness.state, "/metrics", None).await;
    assert_eq!(metrics_status, StatusCode::OK);
    for line in [
        "peryx_pages_served_total{ecosystem=\"pypi\",role=\"cached\"} 1",
        "peryx_artifacts_served_total{ecosystem=\"pypi\",role=\"cached\"} 1",
        "peryx_artifacts_served_bytes_total{ecosystem=\"pypi\",role=\"cached\"} 12",
        "peryx_metadata_served_total{ecosystem=\"pypi\",role=\"cached\"} 1",
    ] {
        assert!(metrics_body.contains(line), "{line} missing from:\n{metrics_body}");
    }

    let (code, _, bytes) = get_bytes_with_headers(&harness.state, "/+status", &credentials).await;
    assert_eq!(code, StatusCode::OK);
    let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let pypi = status["by_ecosystem"]
        .as_array()
        .unwrap()
        .iter()
        .find(|summary| summary["ecosystem"] == "pypi")
        .unwrap();
    assert_eq!(pypi["pages"], 1);
    assert_eq!(pypi["families"]["metadata"], 1);
    assert_eq!(status["metric_families"][0]["key"], "metadata");
    assert_eq!(status["metric_families"][0]["label"], "PEP 658 metadata hits");
}

#[tokio::test]
async fn test_cache_hit_download_records_a_daily_version_bucket() {
    let harness = harness().await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    harness.state.serving.blobs.put_bytes_as(wheel, &digest).await.unwrap();
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());

    let (status, ..) = get(&harness.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    settle(&harness.state.serving.metrics, |metrics| {
        !metrics.daily_usage().is_empty()
    });
    let usage = harness.state.serving.metrics.daily_usage();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].repository, "pypi");
    assert_eq!(usage[0].resource, "flask");
    assert_eq!(usage[0].group, "1.0");

    assert_eq!(usage[0].source, "");
    assert_eq!(usage[0].reads, 1);
    assert_eq!(usage[0].bytes, wheel.len() as u64);
}

#[tokio::test]
async fn test_ranged_download_counts_only_the_transmitted_bytes() {
    let harness = harness().await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    harness.state.serving.blobs.put_bytes_as(wheel, &digest).await.unwrap();
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());

    let (status, ..) = get_bytes_with_headers(&harness.state, &uri, &[("range", "bytes=2-5")]).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    settle(&harness.state.serving.metrics, |metrics| {
        metrics
            .index_totals()
            .get("pypi")
            .is_some_and(|totals| totals.base.reads == 1)
    });
    assert_eq!(harness.state.serving.metrics.index_totals()["pypi"].base.bytes, 4);
}
use crate::tests::http::placement_harness;
