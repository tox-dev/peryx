use peryx_core::UiDescription;
use rstest::rstest;

use crate::markdown::render_description;
use crate::model::{UiSearchPage, UiSnapshot, members_from_listing, projects_from_list};

#[test]
fn test_snapshot_from_status_roundtrip() {
    let value = serde_json::json!({
        "version": "0.0.1",
        "serial": 7,
        "requests": 12,
        "by_ecosystem": [
            {"ecosystem": "pypi", "pages": 12, "downloads": 4, "bytes": 900, "rejected": 0,
             "uploads": 0, "families": {"metadata": 3}}
        ],
        "metric_families": [
            {"key": "metadata", "label": "PEP 658 metadata hits", "roles": ["cached", "hosted", "virtual"]}
        ],
        "indexes": [{
            "name": "pypi",
            "route": "pypi",
            "ecosystem": "pypi",
            "kind": "cached",
            "layers": [],
            "uploads": false,
            "upstream": {"url": "https://pypi.org/simple/", "auth": {"kind": "none"}, "status": "configured"},
            "project_count": 2,
            "upload_count": 0,
            "recent_uploads": [],
        }],
    });
    let snapshot = UiSnapshot::from_status(&value);
    assert_eq!(snapshot.version, "0.0.1");
    assert_eq!(snapshot.serial, 7);
    assert_eq!(snapshot.requests, 12);
    assert_eq!(snapshot.ecosystems.len(), 1);
    assert_eq!(snapshot.ecosystems[0].ecosystem, "pypi");
    assert_eq!(snapshot.ecosystems[0].families["metadata"], 3);
    assert_eq!(snapshot.families[0].label, "PEP 658 metadata hits");
    assert_eq!(snapshot.indexes.len(), 1);
    assert_eq!(snapshot.indexes[0].kind, "cached");
    assert_eq!(snapshot.indexes[0].project_count, 2);
    assert_eq!(
        snapshot.indexes[0].upstream.as_ref().unwrap().url,
        "https://pypi.org/simple/"
    );
}

#[test]
fn test_projects_and_members_from_json() {
    let list = serde_json::json!({"projects": [{"name": "a"}, {"name": "b"}]});
    assert_eq!(projects_from_list(&list), ["a", "b"]);
    let listing =
        serde_json::json!({"members": [{"path": "x/METADATA", "size": 5, "kind": "text", "previewable": true}]});
    let members = members_from_listing(&listing);
    assert_eq!(members[0].path, "x/METADATA");
    assert_eq!(members[0].size, 5);
    assert_eq!(members[0].kind, "text");
    assert!(members[0].previewable);
}

#[test]
fn test_search_page_from_json() {
    let value = serde_json::json!({
        "query": "flask",
        "type": "override",
        "page": 2,
        "page_size": 50,
        "total": 51,
        "results": [{
            "display_name": "Flask",
            "normalized_name": "flask",
            "route": "root/pypi",
                        "type": "override",
            "summary": "web framework",
        }],
    });
    let page = UiSearchPage::from_search(&value);
    assert_eq!(page.query, "flask");
    assert_eq!(page.page, 2);
    assert_eq!(page.results[0].source_label(), "Override");
    assert_eq!(page.results[0].summary.as_deref(), Some("web framework"));
}

#[test]
fn test_render_description_markdown_escapes_inline_html() {
    let html = render_description(&UiDescription {
        text: "# Hi\n\n<script>alert(1)</script>\n\n**bold**".to_owned(),
        content_type: Some("text/markdown".to_owned()),
    });
    assert!(html.contains("<h1>Hi</h1>"));
    assert!(html.contains("<strong>bold</strong>"));
    assert!(!html.contains("<script>"), "inline HTML must be escaped, not executed");
    assert!(html.contains("&lt;script&gt;"));
}

#[rstest]
#[case::javascript("JaVaScRiPt:alert(1)")]
#[case::data("data:text/html;base64,PHNjcmlwdD4=")]
#[case::malformed("http://[invalid")]
fn test_render_description_markdown_removes_unsafe_link_target(#[case] target: &str) {
    let html = render_description(&UiDescription {
        text: format!("[unsafe]({target})"),
        content_type: Some("text/markdown".to_owned()),
    });
    assert_eq!(html, "<p>unsafe</p>\n");
}

#[test]
fn test_render_description_markdown_removes_unsafe_image_target() {
    let html = render_description(&UiDescription {
        text: "![payload](data:image/svg+xml;base64,PHN2Zz4=)".to_owned(),
        content_type: Some("text/markdown".to_owned()),
    });
    assert_eq!(html, "<p>payload</p>\n");
}

#[test]
fn test_render_description_markdown_preserves_safe_image() {
    let html = render_description(&UiDescription {
        text: "![payload](https://example.com/image.svg)".to_owned(),
        content_type: Some("text/markdown".to_owned()),
    });
    assert_eq!(
        html,
        "<p><img src=\"https://example.com/image.svg\" alt=\"payload\" /></p>\n"
    );
}

#[rstest]
#[case::http("http://example.com/docs")]
#[case::https("https://example.com/docs")]
#[case::mailto("mailto:maintainer@example.com")]
#[case::relative("../docs/")]
#[case::fragment("#usage")]
fn test_render_description_markdown_preserves_safe_link(#[case] target: &str) {
    let html = render_description(&UiDescription {
        text: format!("[docs]({target})"),
        content_type: Some("text/markdown".to_owned()),
    });
    assert_eq!(
        html,
        format!("<p><a rel=\"external nofollow noopener noreferrer\" href=\"{target}\">docs</a></p>\n")
    );
}

#[test]
fn test_render_description_plain_text_preformatted() {
    let html = render_description(&UiDescription {
        text: "plain <text>".to_owned(),
        content_type: Some("text/x-rst".to_owned()),
    });
    assert!(html.starts_with("<pre class=\"description-plain\">"));
    assert!(html.contains("plain &lt;text&gt;"));
}

#[test]
fn test_stats_routes_sums_totals_and_sorts_busiest_first() {
    let value = serde_json::json!({
        "hosted": {"base": {"pages": 1, "downloads": 0, "bytes": 10}, "hosted": {"uploads": 2}},
        "root/pypi": {
            "base": {"pages": 5, "downloads": 3, "bytes": 900},
            "cached": {"refreshes": 2, "changed": 1}
        },
    });
    let stats = crate::model::stats_routes(&value);
    assert_eq!(stats.totals.pages, 6);
    assert_eq!(stats.totals.bytes, 910);
    assert_eq!(stats.totals.uploads, 2);
    assert_eq!(stats.totals.changed, 1);
    assert_eq!(stats.rows[0].0, "root/pypi");
    assert_eq!(stats.rows[1].0, "hosted");
}

#[test]
fn test_stats_index_reads_totals_and_projects() {
    let value = serde_json::json!({
        "totals": {
            "base": {"pages": 4, "downloads": 2, "rejected": 1},
            "cached": {"stale_served": 1, "upstream_errors": 1}
        },
        "projects": {
            "pandas": {"base": {"pages": 3, "downloads": 2, "bytes": 500}},
            "six": {"base": {"pages": 1, "downloads": 0}},
        },
    });
    let stats = crate::model::stats_index(&value);
    assert_eq!(stats.totals.stale_served, 1);
    assert_eq!(stats.totals.upstream_errors, 1);
    assert_eq!(stats.totals.rejected, 1);
    assert_eq!(stats.rows[0].0, "pandas");
    assert_eq!(stats.rows[0].1.bytes, 500);
}

#[test]
fn test_stats_project_reads_grouped_totals_and_files() {
    let value = serde_json::json!({
        "totals": {
            "base": {"pages": 3, "downloads": 2, "bytes": 500},
            "ecosystem": {"metadata": 2}
        },
        "files": {
            "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl":
                {"downloads": 2, "bytes": 500, "ecosystem": {"metadata": 2}},
        },
    });
    let stats = crate::model::stats_project(&value);
    assert_eq!(stats.totals.downloads, 2);
    assert_eq!(stats.totals.metadata, 2);
    assert_eq!(stats.rows.len(), 1);
    assert_eq!(stats.rows[0].1.metadata, 2);
}
