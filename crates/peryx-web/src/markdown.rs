//! Long-description rendering for project pages.
//!
//! Package authors control descriptions, so the renderer shows embedded HTML as text and accepts
//! HTTP, HTTPS, mailto, or relative destinations.

use peryx_core::UiDescription;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};
use url::{ParseError, Url};

pub(crate) const EXTERNAL_LINK_REL: &str = "external nofollow noopener noreferrer";

/// Render a long description to safe HTML.
///
/// Markdown is rendered when the document declares `text/markdown` (or declares nothing, which
/// pypi.org treats as markdown-friendly plain text); other content types are shown as preformatted
/// text.
#[must_use]
pub fn render_description(description: &UiDescription) -> String {
    let content_type = description.content_type.as_deref().unwrap_or("text/markdown");
    if content_type.starts_with("text/markdown") {
        render_markdown(&description.text)
    } else {
        format!("<pre class=\"description-plain\">{}</pre>", escape(&description.text))
    }
}

fn render_markdown(text: &str) -> String {
    let mut link_safe = None;
    let parser =
        Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).filter_map(move |event| {
            match event {
                Event::Start(Tag::Link { ref dest_url, .. }) => {
                    let safe = is_safe_link(dest_url);
                    link_safe = Some(safe);
                    safe.then_some(event)
                }
                Event::End(TagEnd::Link) => link_safe.take().unwrap().then_some(event),
                Event::Start(Tag::Image { ref dest_url, .. }) if !is_safe_link(dest_url) => None,
                // Render package HTML as text because package authors control it.
                Event::Html(html) | Event::InlineHtml(html) => Some(Event::Text(html)),
                other => Some(other),
            }
        });
    let mut out = String::with_capacity(text.len());
    html::push_html(&mut out, parser);
    if out.contains("<a href=") {
        out.replace("<a href=", &format!("<a rel=\"{EXTERNAL_LINK_REL}\" href="))
    } else {
        out
    }
}

pub(crate) fn is_safe_link(target: &str) -> bool {
    match Url::parse(target) {
        Ok(url) => matches!(url.scheme(), "http" | "https" | "mailto"),
        Err(ParseError::RelativeUrlWithoutBase) => true,
        Err(_) => false,
    }
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
