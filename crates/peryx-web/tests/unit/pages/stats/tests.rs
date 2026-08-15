use leptos::prelude::*;

use crate::model::{stats_index, stats_resource, stats_routes};

use super::StatsBody;

#[test]
fn stats_body_renders_each_drill_level() {
    let value = serde_json::json!({
        "totals": {"base": {"pages": 4, "reads": 2, "bytes": 2048, "writes": 1}},
        "routes": {"root/cache": {"base": {"pages": 4}}},
        "resources": {"artifact": {"base": {"pages": 4}}},
        "artifacts": {"artifact.bin": {"reads": 2, "bytes": 2048}},
    });
    for (route, resource, label, breadcrumb) in [
        (None, None, "Index", r#"<p class="breadcrumb"><span>usage</span></p>"#),
        (
            Some("root/cache"),
            None,
            "Resource",
            r#"<p class="breadcrumb"><a href="/stats">usage</a> / <span>root/cache</span></p>"#,
        ),
        (
            Some("root/cache"),
            Some("artifact"),
            "Artifact",
            r#"<a href="/stats?index=root%2Fcache">root/cache</a> / <span>artifact</span>"#,
        ),
    ] {
        let data = match (route, resource) {
            (Some(_), Some(_)) => stats_resource(&value),
            (Some(_), None) => stats_index(&value),
            _ => stats_routes(&value),
        };
        let html = view! {
            <StatsBody route=route.map(str::to_owned) resource=resource.map(str::to_owned) data />
        }
        .to_html();
        assert!(html.contains(breadcrumb), "{html}");
        assert!(html.contains(label), "{html}");
        assert!(html.contains("Listings"), "{html}");
        assert!(html.contains("2.0 kB"), "{html}");
    }
}

#[test]
fn stats_body_reports_empty_level() {
    let html = view! { <StatsBody route=None resource=None data=stats_routes(&serde_json::json!({})) /> }.to_html();
    assert!(html.contains("Nothing recorded at this level yet."), "{html}");
}
