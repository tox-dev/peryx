//! Long-description rendering for project pages.
//!
//! Descriptions come from arbitrary packages, so inline HTML is escaped rather than passed through:
//! markdown formatting works, embedded tags render as text.

use pulldown_cmark::{Event, Options, Parser, html};
use velodex_core::pypi::CoreMetadataDoc;

/// Render a long description to safe HTML.
///
/// Markdown is rendered when the document declares `text/markdown` (or declares nothing, which
/// pypi.org treats as markdown-friendly plain text); other content types are shown as preformatted
/// text.
#[must_use]
pub fn render_description(doc: &CoreMetadataDoc) -> String {
    let content_type = doc.description_content_type.as_deref().unwrap_or("text/markdown");
    if content_type.starts_with("text/markdown") {
        render_markdown(&doc.description)
    } else {
        format!("<pre class=\"description-plain\">{}</pre>", escape(&doc.description))
    }
}

fn render_markdown(text: &str) -> String {
    let parser = Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).map(|event| {
        match event {
            // Inline or block HTML from a package description is untrusted: show it, do not run it.
            Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
            other => other,
        }
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}
