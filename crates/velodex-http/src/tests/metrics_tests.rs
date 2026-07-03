use crate::metrics::{Event, Metrics};

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
        filename: "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl".into(),
        bytes: 100,
    });
    metrics.record(Event::Download {
        route: "root/pypi".into(),
        filename: "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl".into(),
        bytes: 50,
    });
    metrics.record(Event::Metadata {
        route: "root/pypi".into(),
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
        filename: "flask-1.0-py3-none-any.whl".into(),
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
