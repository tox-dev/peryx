use rstest::rstest;

use crate::description::render;

#[test]
fn test_render_markdown_escapes_inline_html() {
    let html = render("# Hi\n\n<script>alert(1)</script>\n\n**bold**", Some("text/markdown")).html;
    assert!(html.contains("<h1>Hi</h1>"));
    assert!(html.contains("<strong>bold</strong>"));
    assert!(!html.contains("<script>"), "inline HTML must be escaped, not executed");
    assert!(html.contains("&lt;script&gt;"));
}

#[test]
fn test_render_absent_content_type_renders_rst() {
    let html = render("Title\n=====\n\n*emphasis*", None).html;
    assert!(html.contains("<em>emphasis</em>"));
}

#[test]
fn test_render_absent_content_type_is_not_markdown() {
    let rendered = render("# Not a heading", None);
    assert!(rendered.html.contains("# Not a heading"));
    assert!(
        !rendered.html.contains("<h1>"),
        "an absent content type is reStructuredText"
    );
    assert!(rendered.notice.is_none());
}

#[test]
fn test_render_rst_link_is_hardened() {
    let html = render("`docs <https://example.com/docs>`_", Some("text/x-rst")).html;
    assert!(
        html.contains("<a href=\"https://example.com/docs\" rel=\"external nofollow noopener noreferrer\">docs</a>")
    );
}

#[rstest]
#[case::raw_html(".. raw:: html\n\n   <script>alert(1)</script>\n")]
#[case::javascript_link("`click <javascript:alert(1)>`_")]
fn test_render_rst_neutralizes_injection(#[case] text: &str) {
    let html = render(text, None).html;
    assert!(
        !html.contains("<script"),
        "package HTML must not reach the page: {html}"
    );
    assert!(
        !html.contains("javascript:"),
        "unsafe destinations must be dropped: {html}"
    );
}

#[test]
fn test_render_rst_failure_falls_back_to_plain_text() {
    let rendered = render("unresolved |substitution| reference", None);
    assert_eq!(
        rendered.html,
        "<pre class=\"description-plain\">unresolved |substitution| reference</pre>"
    );
    assert!(rendered.notice.is_some_and(|notice| notice.contains("plain text")));
}

#[rstest]
#[case::javascript("JaVaScRiPt:alert(1)")]
#[case::data("data:text/html;base64,PHNjcmlwdD4=")]
#[case::malformed("http://[invalid")]
fn test_render_markdown_removes_unsafe_link_target(#[case] target: &str) {
    let html = render(&format!("[unsafe]({target})"), Some("text/markdown")).html;
    assert_eq!(html, "<p>unsafe</p>\n");
}

#[test]
fn test_render_markdown_removes_unsafe_image_target() {
    let html = render("![payload](data:image/svg+xml;base64,PHN2Zz4=)", Some("text/markdown")).html;
    assert_eq!(html, "<p>payload</p>\n");
}

#[test]
fn test_render_markdown_preserves_safe_image() {
    let html = render("![payload](https://example.com/image.svg)", Some("text/markdown")).html;
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
fn test_render_markdown_preserves_safe_link(#[case] target: &str) {
    let html = render(&format!("[docs]({target})"), Some("text/markdown")).html;
    assert_eq!(
        html,
        format!("<p><a rel=\"external nofollow noopener noreferrer\" href=\"{target}\">docs</a></p>\n")
    );
}

#[test]
fn test_render_plain_text_preformatted() {
    let html = render("plain <text> & more", Some("text/plain")).html;
    assert!(html.starts_with("<pre class=\"description-plain\">"));
    assert!(html.contains("plain &lt;text&gt; &amp; more"));
}
