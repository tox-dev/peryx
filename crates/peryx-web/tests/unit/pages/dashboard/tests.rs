use leptos::prelude::*;

use crate::model::{UiCounters, UiIndex, UiSnapshot, UiStats, UiSummaryStatus};

use super::{DashboardBody, OverlayCard};

fn index(name: &str, kind: &str, layers: Vec<String>) -> UiIndex {
    UiIndex {
        name: name.to_owned(),
        route: format!("root/{name}"),
        ecosystem: "example".to_owned(),
        endpoint: format!("/{name}/"),
        kind: kind.to_owned(),
        layers,
        uploads: true,
        upload_to: None,
        upstream: None,
        hosted: None,
        summary_status: UiSummaryStatus::Available,
        summary_error_class: None,
        resource_count: 0,
        write_count: 0,
        recent_writes: Vec::new(),
    }
}

#[test]
fn dashboard_body_renders_overlay_and_standalone_cards() {
    let hosted = index("hosted", "hosted", Vec::new());
    let standalone = index("standalone", "cached", Vec::new());
    let mut quiet = index("quiet", "cached", Vec::new());
    quiet.uploads = false;
    let mut overlay = index("public", "virtual", vec!["hosted".to_owned(), "missing".to_owned()]);
    overlay.upload_to = Some("hosted".to_owned());
    let mut quiet_overlay = index("quiet-public", "virtual", vec!["hosted".to_owned()]);
    quiet_overlay.uploads = false;
    let html = view! {
        <DashboardBody
            data=UiSnapshot {
                version: "1.2.3".to_owned(),
                serial: Some(4),
                requests: 5,
                indexes: vec![hosted, standalone, quiet, overlay, quiet_overlay],
                ..UiSnapshot::default()
            }
            usage=Some(UiStats {
                totals: UiCounters::default(),
                rows: vec![
                    ("root/public".to_owned(), UiCounters { pages: 2, bytes: 1_536, ..UiCounters::default() }),
                    ("root/standalone".to_owned(), UiCounters { reads: 3, ..UiCounters::default() }),
                ],
            })
        />
    }
    .to_html();
    assert!(html.contains("writes land here"), "{html}");
    assert!(html.contains("kind-?"), "{html}");
    assert!(html.contains("Standalone indexes"), "{html}");
    assert!(html.contains("listings"), "{html}");
    assert!(html.contains("1.5 kB"), "{html}");
    assert_eq!(html.matches(r#"class="card-usage""#).count(), 2, "{html}");
    let empty = view! { <DashboardBody data=UiSnapshot::default() usage=Some(UiStats::default()) /> }.to_html();
    assert!(empty.contains("Indexes"), "{empty}");
    assert!(!empty.contains("Standalone indexes"), "{empty}");
}

fn overlay_html(upload_to: Option<&str>) -> String {
    let mut overlay = index("overlay", "virtual", vec!["member".to_owned()]);
    overlay.upload_to = upload_to.map(str::to_owned);
    let member = index("member", "hosted", Vec::new());

    view! { <OverlayCard index=overlay all=vec![member] counters=None /> }.to_html()
}

/// A layer is named, and the card reports the kind of the member wearing that name. Matching any
/// other member would label the layer with a kind that is not its own.
#[test]
fn overlay_card_labels_a_layer_with_its_own_members_kind() {
    let html = overlay_html(None);

    assert!(html.contains("badge kind-hosted"), "{html}");
    assert!(!html.contains(">?<"), "an unmatched layer falls back to '?': {html}");
}

/// Writes land on one named layer, so only the layer that name points at carries the marker.
#[test]
fn overlay_card_marks_only_the_named_upload_target() {
    assert!(overlay_html(Some("member")).contains("writes land here"));
}

/// A layer that is not the upload target goes unmarked, which is what makes the marker mean anything.
#[test]
fn overlay_card_leaves_a_layer_that_is_not_the_target_unmarked() {
    assert!(!overlay_html(Some("elsewhere")).contains("writes land here"));
}
