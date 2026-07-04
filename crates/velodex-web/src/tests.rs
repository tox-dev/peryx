use velodex_ecosystem_pypi::parse_metadata;

use crate::markdown::render_description;
use crate::model::{UiProject, UiSearchPage, UiSnapshot, members_from_listing, projects_from_list};

#[test]
fn test_snapshot_from_status_roundtrip() {
    let value = serde_json::json!({
        "version": "0.0.1",
        "serial": 7,
        "requests": 12,
        "metadata_requests": 3,
        "indexes": [{
            "name": "pypi",
            "route": "pypi",
            "kind": "mirror",
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
    assert_eq!(snapshot.indexes.len(), 1);
    assert_eq!(snapshot.indexes[0].kind, "mirror");
    assert_eq!(snapshot.indexes[0].project_count, 2);
    assert_eq!(
        snapshot.indexes[0].upstream.as_ref().unwrap().url,
        "https://pypi.org/simple/"
    );
}

#[test]
fn test_project_from_detail_maps_files() {
    let value = serde_json::json!({
        "name": "veloxdemo",
        "versions": ["1.0"],
        "files": [{
            "filename": "veloxdemo-1.0-py3-none-any.whl",
            "url": "/local/files/aa/veloxdemo-1.0-py3-none-any.whl",
            "hashes": {"sha256": "aa"},
            "size": 10,
            "upload-time": "2026-01-01T00:00:00Z",
            "yanked": "broken",
            "core-metadata": {"sha256": "bb"},
        }],
    });
    let project = UiProject::from_detail(&value);
    assert_eq!(project.name, "veloxdemo");
    assert_eq!(project.files[0].sha256, "aa");
    assert_eq!(project.files[0].upload_time.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert!(project.files[0].yanked, "a reason string counts as yanked");
    assert!(project.files[0].has_metadata);
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
        "type": "upstream-overrides",
        "page": 2,
        "page_size": 50,
        "total": 51,
        "results": [{
            "display_name": "Flask",
            "normalized_name": "flask",
            "route": "root/pypi",
            "repository": "root/pypi",
            "type": "upstream-overrides",
            "summary": "web framework",
        }],
    });
    let page = UiSearchPage::from_search(&value);
    assert_eq!(page.query, "flask");
    assert_eq!(page.page, 2);
    assert_eq!(page.results[0].source_label(), "Upstream+");
    assert_eq!(page.results[0].summary.as_deref(), Some("web framework"));
}

#[test]
fn test_render_description_markdown_escapes_inline_html() {
    let doc = parse_metadata(
        "Name: x\nVersion: 1\nDescription-Content-Type: text/markdown\n\n# Hi\n\n<script>alert(1)</script>\n\n**bold**",
    );
    let html = render_description(&doc);
    assert!(html.contains("<h1>Hi</h1>"));
    assert!(html.contains("<strong>bold</strong>"));
    assert!(!html.contains("<script>"), "inline HTML must be escaped, not executed");
    assert!(html.contains("&lt;script&gt;"));
}

#[test]
fn test_render_description_plain_text_preformatted() {
    let doc = parse_metadata("Name: x\nVersion: 1\nDescription-Content-Type: text/x-rst\n\nplain <text>");
    let html = render_description(&doc);
    assert!(html.starts_with("<pre class=\"description-plain\">"));
    assert!(html.contains("plain &lt;text&gt;"));
}

#[test]
fn test_stats_routes_sums_totals_and_sorts_busiest_first() {
    let value = serde_json::json!({
        "local": {"pages": 1, "downloads": 0, "bytes": 10, "uploads": 2},
        "root/pypi": {"pages": 5, "downloads": 3, "bytes": 900, "refreshes": 2, "changed": 1},
    });
    let stats = crate::model::stats_routes(&value);
    assert_eq!(stats.totals.pages, 6);
    assert_eq!(stats.totals.bytes, 910);
    assert_eq!(stats.totals.uploads, 2);
    assert_eq!(stats.totals.changed, 1);
    assert_eq!(stats.rows[0].0, "root/pypi");
    assert_eq!(stats.rows[1].0, "local");
}

#[test]
fn test_stats_index_reads_totals_and_projects() {
    let value = serde_json::json!({
        "totals": {"pages": 4, "downloads": 2, "stale_served": 1, "upstream_errors": 1, "rejected": 1},
        "projects": {
            "pandas": {"pages": 3, "downloads": 2, "bytes": 500},
            "six": {"pages": 1, "downloads": 0},
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
fn test_stats_project_reads_flattened_totals_and_files() {
    let value = serde_json::json!({
        "pages": 3, "downloads": 2, "metadata": 2, "uploads": 0, "bytes": 500,
        "files": {
            "pandas-3.0.3-cp314-cp314-macosx_11_0_arm64.whl": {"downloads": 2, "metadata": 2, "bytes": 500},
        },
    });
    let stats = crate::model::stats_project(&value);
    assert_eq!(stats.totals.downloads, 2);
    assert_eq!(stats.rows.len(), 1);
    assert_eq!(stats.rows[0].1.metadata, 2);
}
