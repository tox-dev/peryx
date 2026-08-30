use leptos::prelude::*;

use crate::model::{UiCounters, UiHosted, UiIndex, UiRecentWrite, UiSnapshot, UiStats, UiSummaryStatus, UiUpstream};

use super::AdminStatusBody;

fn index(name: &str, kind: &str) -> UiIndex {
    UiIndex {
        name: name.to_owned(),
        route: format!("root/{name}"),
        ecosystem: "example".to_owned(),
        endpoint: format!("/{name}/"),
        kind: kind.to_owned(),
        layers: Vec::new(),
        uploads: false,
        upload_to: None,
        upstream: None,
        hosted: None,
        summary_status: UiSummaryStatus::Available,
        summary_error_class: None,
        resource_count: 2,
        write_count: 3,
        recent_writes: Vec::new(),
    }
}

#[test]
fn admin_status_body_renders_index_states_and_usage() {
    let mut cached = index("cache", "cached");
    cached.upstream = Some(UiUpstream {
        url: "https://example.test/catalog".to_owned(),
        auth_kind: "basic".to_owned(),
        auth_redacted: Some("user:***".to_owned()),
        status: "ready".to_owned(),
    });
    let mut hosted = index("hosted", "hosted");
    hosted.uploads = true;
    hosted.hosted = Some(UiHosted {
        volatile: false,
        token_configured: true,
        token_redacted: Some("***".to_owned()),
    });
    hosted.recent_writes = vec![
        UiRecentWrite {
            resource: "artifact".to_owned(),
            artifact: "artifact.bin".to_owned(),
            group: "1.0".to_owned(),
            written_at: Some("2026-08-08T12:00:00Z".to_owned()),
            size: Some(1_536),
        },
        UiRecentWrite {
            resource: "pending".to_owned(),
            artifact: "pending.bin".to_owned(),
            group: "2.0".to_owned(),
            written_at: None,
            size: None,
        },
    ];
    let mut virtual_index = index("public", "virtual");
    virtual_index.layers = vec!["hosted".to_owned(), "missing".to_owned()];
    virtual_index.upload_to = Some("hosted".to_owned());
    let html = view! {
        <AdminStatusBody
            data=UiSnapshot {
                version: "1.2.3".to_owned(),
                serial: 4,
                requests: 5,
                indexes: vec![cached, hosted, virtual_index],
                ..UiSnapshot::default()
            }
            usage=Some(UiStats {
                totals: UiCounters { pages: 1, ..UiCounters::default() },
                rows: vec![(
                    "root/hosted".to_owned(),
                    UiCounters { pages: 2, reads: 3, bytes: 1_536, ..UiCounters::default() },
                )],
            })
        />
    }
    .to_html();
    assert!(html.contains("basic auth"), "{html}");
    assert!(html.contains("non-volatile"), "{html}");
    assert!(html.contains("artifact.bin"), "{html}");
    assert!(html.contains("Listings"), "{html}");
    assert!(html.contains("1.5 kB"), "{html}");
    assert!(html.contains(">missing</span>"), "{html}");
    assert_eq!(html.matches(">n/a</td>").count(), 2, "{html}");
    let mut unavailable = index("degraded", "hosted");
    unavailable.summary_status = UiSummaryStatus::Unavailable;
    unavailable.summary_error_class = Some("storage".to_owned());
    let unavailable = view! {
        <AdminStatusBody
            data=UiSnapshot { indexes: vec![unavailable], ..UiSnapshot::default() }
            usage=Some(UiStats::default())
        />
    }
    .to_html();
    assert_eq!(unavailable.matches(">unavailable<").count(), 4, "{unavailable}");
    let empty = view! { <AdminStatusBody data=UiSnapshot::default() usage=Some(UiStats::default()) /> }.to_html();
    for expected in [
        "No indexes configured.",
        "No writes recorded yet.",
        "No usage recorded yet.",
    ] {
        assert!(empty.contains(expected), "missing {expected:?} in {empty}");
    }
}

#[test]
fn admin_status_body_renders_auth_and_storage_fallbacks() {
    let mut bearer = index("secured", "cached");
    bearer.upstream = Some(UiUpstream {
        url: "https://example.test/bearer".to_owned(),
        auth_kind: "bearer".to_owned(),
        auth_redacted: None,
        status: "ready".to_owned(),
    });
    let mut anonymous = index("open", "cached");
    anonymous.upstream = Some(UiUpstream {
        url: "https://example.test/anonymous".to_owned(),
        auth_kind: "none".to_owned(),
        auth_redacted: None,
        status: "ready".to_owned(),
    });
    let mut volatile = index("ephemeral", "hosted");
    volatile.hosted = Some(UiHosted {
        volatile: true,
        token_configured: false,
        token_redacted: None,
    });
    let mut composed = index("composed", "virtual");
    composed.layers = vec!["ephemeral".to_owned()];
    let html = view! {
        <AdminStatusBody
            data=UiSnapshot {
                indexes: vec![bearer, anonymous, volatile, composed],
                ..UiSnapshot::default()
            }
            usage=Some(UiStats::default())
        />
    }
    .to_html();
    for expected in [
        "bearer auth",
        "anonymous",
        ">volatile</span>",
        "no write token",
        "composed from layers",
    ] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}
