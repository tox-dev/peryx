//! Rendering a project's long description to safe HTML on the server.
//!
//! Package authors control descriptions, so the renderer keeps embedded HTML off the page and accepts
//! only HTTP, HTTPS, mailto, or relative destinations. It runs here, in the driver, rather than in the
//! browser: the reStructuredText renderer aborts on nodes it never implemented, and that abort cannot
//! be caught in WebAssembly, so the browser receives the finished HTML instead of the source.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

use ammonia::Builder;
use peryx_core::RenderedDescription;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};
use url::{ParseError, Url};

const EXTERNAL_LINK_REL: &str = "external nofollow noopener noreferrer";

/// Render a long description to safe HTML.
///
/// Markdown is rendered when the document declares `text/markdown`, reStructuredText when it declares
/// `text/x-rst` or declares nothing, which is the default Core Metadata mandates. Other content types,
/// and reStructuredText that fails to render, are shown as preformatted text.
#[must_use]
pub fn render(text: &str, content_type: Option<&str>) -> RenderedDescription {
    let content_type = content_type.unwrap_or("text/x-rst");
    if content_type.starts_with("text/markdown") {
        RenderedDescription {
            html: render_markdown(text),
            notice: None,
        }
    } else if content_type.starts_with("text/x-rst") {
        rst_html(text).map_or_else(
            || RenderedDescription {
                html: render_plain(text),
                notice: Some(
                    "This description is not valid reStructuredText, so it is shown as plain text.".to_owned(),
                ),
            },
            |html| RenderedDescription { html, notice: None },
        )
    } else {
        RenderedDescription {
            html: render_plain(text),
            notice: None,
        }
    }
}

/// The renderer panics on document nodes it does not implement, such as substitution references, and
/// package authors control the source, so a panic here is bad input rather than a bug.
fn rst_html(text: &str) -> Option<String> {
    let render = || {
        let document = rst_parser::parse(text).ok()?;
        let mut out = Vec::with_capacity(text.len());
        rst_renderer::render_html(&document, &mut out, false).ok()?;
        String::from_utf8(out).ok()
    };
    let html = catch_unwind(AssertUnwindSafe(render)).ok().flatten()?;
    // The renderer emits `raw` directives verbatim and does not restrict destinations, so the
    // sanitizer, not the renderer, is what keeps author HTML off the page.
    Some(
        Builder::new()
            .url_schemes(HashSet::from(["http", "https", "mailto"]))
            .link_rel(Some(EXTERNAL_LINK_REL))
            .clean(&html)
            .to_string(),
    )
}

fn render_plain(text: &str) -> String {
    format!("<pre class=\"description-plain\">{}</pre>", escape(text))
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

/// A markdown link is kept only when its scheme is one a browser can follow safely; a relative link
/// stays inside the site. The web layer applies the same rule to the links it renders itself.
fn is_safe_link(target: &str) -> bool {
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
