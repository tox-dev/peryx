use leptos::prelude::*;
use peryx_core::{
    BrowseBadge, BrowseCell, BrowseLink, BrowsePage, BrowseProperty, BrowseRow, BrowseSection, UiAction, UiActionMethod,
};

use super::{BrowseBadges, BrowseDocument, BrowseLinks, BrowsePropertyView, BrowseSectionView, BrowseTable};

#[tokio::test(flavor = "current_thread")]
async fn browse_page_renders_present_and_absent_fields() {
    match any_spawner::Executor::init_tokio() {
        Ok(()) | Err(any_spawner::ExecutorError::AlreadySet) => {}
    }
    let html = render_document(BrowsePage {
        breadcrumbs: vec![
            BrowseLink {
                label: "Home".to_owned(),
                href: "/browse".to_owned(),
            },
            BrowseLink {
                label: "Package".to_owned(),
                href: "https://example.test/package".to_owned(),
            },
        ],
        title: "Artifact & package".to_owned(),
        subtitle: Some("Build output".to_owned()),
        summary: Some("Signed release".to_owned()),
        command: Some("fetch <artifact>".to_owned()),
        badges: vec![BrowseBadge {
            label: "local".to_owned(),
            class: "available".to_owned(),
            hint: Some("Stored here".to_owned()),
        }],
        sections: vec![BrowseSection::Markup {
            heading: "Details".to_owned(),
            html: "<strong>trusted</strong>".to_owned(),
            notice: None,
        }],
        actions: vec![
            action("Refresh", UiActionMethod::Post, false),
            action("Remove", UiActionMethod::Delete, true),
        ],
    });
    assert_contains(
        &html,
        &[
            r#"<nav class="breadcrumb">"#,
            r#"href="/browse">Home</a> / <a href="https://example.test/package" rel="external nofollow noopener noreferrer">Package</a>"#,
            "<h1>Artifact &amp; package</h1>",
            r#"<p class="browse-subtitle">Build output</p>"#,
            r#"<p class="summary">Signed release</p>"#,
            "<code>fetch &lt;artifact&gt;</code>",
            r#"<span title="Stored here" class="badge available">local</span>"#,
            "<h2>Details</h2>",
            r#"<details class="admin">"#,
            r#"autocomplete="username" placeholder="Username""#,
            r#"type="password" placeholder="Password" class="token""#,
            ">Refresh</button>",
            r#"class="danger">Remove</button>"#,
        ],
    );
    assert_eq!(html.matches(r#"class="danger""#).count(), 1, "{html}");

    let minimal = render_document(BrowsePage {
        title: "Minimal".to_owned(),
        ..BrowsePage::default()
    });
    assert_contains(&minimal, &["<h1>Minimal</h1>"]);
    for absent in [
        "breadcrumb",
        "browse-subtitle",
        "summary",
        "install",
        "admin",
        "outcome",
    ] {
        assert!(!minimal.contains(absent), "unexpected {absent:?} in {minimal}");
    }
}

#[test]
fn browse_sections_render_each_shape() {
    let cases: [(BrowseSection, &[&str], &[&str]); 4] = [
        (
            BrowseSection::Markup {
                heading: "Description".to_owned(),
                html: "<em>trusted</em>".to_owned(),
                notice: Some("Sanitized upstream".to_owned()),
            },
            &[
                "<h2>Description</h2>",
                r#"<p class="dim">Sanitized upstream</p>"#,
                r#"<div class="description"><em>trusted</em></div>"#,
            ],
            &[],
        ),
        (
            BrowseSection::Markup {
                heading: "Readme".to_owned(),
                html: "<strong>body</strong>".to_owned(),
                notice: None,
            },
            &[
                "<h2>Readme</h2>",
                r#"<div class="description"><strong>body</strong></div>"#,
            ],
            &[r#"class="dim""#],
        ),
        (
            BrowseSection::Properties {
                heading: "Properties".to_owned(),
                entries: vec![BrowseProperty {
                    label: "Digest".to_owned(),
                    value: "abc123".to_owned(),
                    href: None,
                }],
            },
            &[
                "<h2>Properties</h2>",
                r#"<dl class="browse-properties">"#,
                "<dt>Digest</dt>",
            ],
            &[],
        ),
        (
            BrowseSection::Links {
                heading: "Related".to_owned(),
                entries: vec![BrowseLink {
                    label: "Metadata".to_owned(),
                    href: "/metadata".to_owned(),
                }],
                empty: "No related records".to_owned(),
            },
            &[
                "<h2>Related</h2>",
                r#"<ul class="links-list">"#,
                r#"href="/metadata">Metadata</a>"#,
            ],
            &["No related records"],
        ),
    ];
    assert_browse_sections(cases);
}

#[test]
fn browse_sections_render_tables_and_content() {
    let cases: [(BrowseSection, &[&str], &[&str]); 3] = [
        (
            BrowseSection::Table {
                heading: "Files".to_owned(),
                columns: vec!["Name".to_owned()],
                rows: vec![BrowseRow {
                    cells: vec![BrowseCell {
                        text: "artifact.bin".to_owned(),
                        href: None,
                        code: false,
                    }],
                    badges: Vec::new(),
                    actions: Vec::new(),
                }],
                empty: "No files".to_owned(),
            },
            &["<h2>Files</h2>", r#"<table class="browse-table">"#, "artifact.bin"],
            &["No files"],
        ),
        (
            BrowseSection::Content {
                heading: "Preview".to_owned(),
                text: "<content>".to_owned(),
                size: Some(1_536),
                offset: 32,
                next: Some(BrowseLink {
                    label: "Next page".to_owned(),
                    href: "/preview?offset=1568".to_owned(),
                }),
            },
            &[
                "<h2>Preview</h2>",
                "1.5 kB at byte 32",
                "<code>&lt;content&gt;</code>",
                r#"href="/preview?offset=1568" class="page-link">Next page</a>"#,
            ],
            &[],
        ),
        (
            BrowseSection::Content {
                heading: "Body".to_owned(),
                text: "content".to_owned(),
                size: None,
                offset: 0,
                next: None,
            },
            &[
                "<h2>Body</h2>",
                r#"<pre class="browse-content"><code>content</code></pre>"#,
            ],
            &["at byte", "page-link"],
        ),
    ];
    assert_browse_sections(cases);
}

fn assert_browse_sections<const N: usize>(cases: [(BrowseSection, &[&str], &[&str]); N]) {
    for (section, expected, absent) in cases {
        let html = view! { <BrowseSectionView section /> }.to_html();
        assert_contains(&html, expected);
        for value in absent {
            assert!(!html.contains(value), "unexpected {value:?} in {html}");
        }
    }
}

#[test]
fn browse_properties_render_link_variants() {
    for (href, expected, external) in [
        (None, r"<span>Value &amp; more</span>", false),
        (Some("/inside"), r#"<a href="/inside">Value &amp; more</a>"#, false),
        (
            Some("https://example.test/value"),
            r#"<a href="https://example.test/value" rel="external nofollow noopener noreferrer">Value &amp; more</a>"#,
            true,
        ),
    ] {
        let html = view! {
            <BrowsePropertyView entry=BrowseProperty {
                label: "Label".to_owned(),
                value: "Value & more".to_owned(),
                href: href.map(str::to_owned),
            } />
        }
        .to_html();
        assert_contains(&html, &["<dt>Label</dt>", expected]);
        assert_eq!(html.contains(r#"rel=""#), external, "{html}");
    }
}

#[test]
fn browse_links_render_empty_and_populated_lists() {
    assert_eq!(
        view! { <BrowseLinks links=Vec::new() empty="No links".to_owned() /> }.to_html(),
        r#"<p class="dim">No links</p>"#,
    );
    let html = view! {
        <BrowseLinks
            links=vec![
                BrowseLink { label: "Internal".to_owned(), href: "/inside".to_owned() },
                BrowseLink { label: "External".to_owned(), href: "//example.test/outside".to_owned() },
            ]
            empty="Unused".to_owned()
        />
    }
    .to_html();
    assert_contains(
        &html,
        &[
            r#"<ul class="links-list">"#,
            r#"href="/inside">Internal</a>"#,
            r#"href="//example.test/outside" rel="external nofollow noopener noreferrer">External</a>"#,
        ],
    );
    assert!(!html.contains("Unused"), "{html}");
}

#[test]
fn browse_badges_render_empty_hint_and_class_variants() {
    let empty = view! { <BrowseBadges badges=Vec::new() /> }.to_html();
    assert!(empty.contains(r#"<div class="browse-badges">"#), "{empty}");
    assert!(!empty.contains("<span"), "{empty}");
    let html = view! {
        <BrowseBadges badges=vec![
            BrowseBadge {
                label: "Available".to_owned(),
                class: "healthy".to_owned(),
                hint: Some("Ready & local".to_owned()),
            },
            BrowseBadge {
                label: "Remote".to_owned(),
                class: "muted".to_owned(),
                hint: None,
            },
        ] />
    }
    .to_html();
    assert_contains(
        &html,
        &[
            r#"<span title="Ready &amp; local" class="badge healthy">Available</span>"#,
            r#"<span class="badge muted">Remote</span>"#,
        ],
    );
}

#[test]
fn browse_cells_render_code_and_link_combinations() {
    for (href, code, expected, external) in [
        (None, false, r"<td><span>cell &amp; value</span></td>", false),
        (None, true, r"<td><code>cell &amp; value</code></td>", false),
        (
            Some("/inside"),
            false,
            r#"<td><a href="/inside"><span>cell &amp; value</span></a></td>"#,
            false,
        ),
        (
            Some("https://example.test/cell"),
            true,
            r#"<td><a href="https://example.test/cell" rel="external nofollow noopener noreferrer"><code>cell &amp; value</code></a></td>"#,
            true,
        ),
    ] {
        let html = view! {
            <super::BrowseCellView cell=BrowseCell {
                text: "cell & value".to_owned(),
                href: href.map(str::to_owned),
                code,
            } />
        }
        .to_html();
        assert_eq!(html, expected);
        assert_eq!(html.contains(r#"rel=""#), external, "{html}");
    }
}

#[test]
fn browse_table_renders_empty_rows_and_optional_row_columns() {
    assert_eq!(
        view! { <BrowseTable columns=vec!["Name".to_owned()] rows=Vec::new() empty="No rows".to_owned() /> }.to_html(),
        r#"<p class="dim">No rows</p>"#,
    );
    let html = view! {
        <BrowseTable
            columns=vec!["Name".to_owned(), "State".to_owned(), "Action".to_owned()]
            rows=vec![
                BrowseRow {
                    cells: vec![BrowseCell { text: "complete".to_owned(), href: None, code: false }],
                    badges: vec![BrowseBadge {
                        label: "ready".to_owned(),
                        class: "healthy".to_owned(),
                        hint: None,
                    }],
                    actions: vec![
                        action("Remove", UiActionMethod::Delete, true),
                        action("Restore", UiActionMethod::Put, false),
                    ],
                },
                BrowseRow {
                    cells: vec![BrowseCell { text: "bare".to_owned(), href: None, code: false }],
                    badges: Vec::new(),
                    actions: Vec::new(),
                },
            ]
            empty="Unused".to_owned()
        />
    }
    .to_html();
    assert_contains(
        &html,
        &[
            r#"<div class="table-scroll"><table class="browse-table">"#,
            "<th>Name</th><th>State</th><th>Action</th>",
            r#"<td><div class="browse-badges"><span class="badge healthy">ready</span>"#,
            "<td>RemoveRestore",
            "<td><span>bare</span></td>",
        ],
    );
    assert!(!html.contains("Unused"), "{html}");
}

fn render_document(document: BrowsePage) -> String {
    Owner::new().with(|| view! { <BrowseDocument document refresh=refresh() /> }.to_html())
}

fn refresh() -> Resource<Result<Option<BrowsePage>, String>> {
    Resource::new(|| (), |()| async { Ok(None) })
}

fn action(label: &str, method: UiActionMethod, destructive: bool) -> UiAction {
    UiAction {
        label: label.to_owned(),
        method,
        endpoint: "/action".to_owned(),
        destructive,
    }
}

fn assert_contains(html: &str, expected: &[&str]) {
    let rendered = html.replace("<!>", "");
    for value in expected {
        assert!(rendered.contains(value), "missing {value:?} in {rendered}");
    }
}
