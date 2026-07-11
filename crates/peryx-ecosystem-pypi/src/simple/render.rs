//! Byte-exact PEP 503 HTML rendering of the served response model.

use std::fmt::Write as _;

use super::{CoreMetadata, Meta, ProjectDetail, ProjectList, Provenance, Yanked};

/// Render the PEP 503 HTML for the root project list. The `href` is the normalized name; the
/// anchor text is the project's display name.
#[must_use]
pub fn render_index_html(list: &ProjectList) -> String {
    let mut out = String::new();
    push_head(&mut out, "Simple index", &list.meta);
    for entry in &list.projects {
        let _ = write!(out, "    <a href=\"{}/\">", crate::normalize_name_cow(&entry.name));
        push_escaped(&mut out, &entry.name, Escape::Text);
        out.push_str("</a>\n");
    }
    push_tail(&mut out);
    out
}

/// Render the PEP 503 HTML for a project detail page.
#[must_use]
pub fn render_detail_html(detail: &ProjectDetail) -> String {
    let mut out = String::with_capacity(detail.files.len() * 256 + 512);
    push_head(&mut out, &format!("Links for {}", detail.name), &detail.meta);
    for file in &detail.files {
        out.push_str("    <a href=\"");
        push_escaped(&mut out, &file.url, Escape::Attr);
        if let Some((algo, hash)) = file
            .hashes
            .get_key_value("sha256")
            .or_else(|| file.hashes.iter().next())
        {
            let _ = write!(out, "#{algo}={hash}");
        }
        out.push('"');
        if let Some(requires_python) = &file.requires_python {
            push_attr(&mut out, " data-requires-python=\"", requires_python);
        }
        if let Some(gpg_sig) = file.gpg_sig {
            let _ = write!(out, " data-gpg-sig=\"{gpg_sig}\"");
        }
        match &file.yanked {
            Yanked::No => {}
            Yanked::Yes => out.push_str(" data-yanked=\"\""),
            Yanked::Reason(reason) => push_attr(&mut out, " data-yanked=\"", reason),
        }
        if let Provenance::Url(url) = &file.provenance {
            push_attr(&mut out, " data-provenance=\"", url);
        }
        push_core_metadata_attr(&mut out, file.metadata());
        out.push('>');
        push_escaped(&mut out, &file.filename, Escape::Text);
        out.push_str("</a><br />\n");
    }
    push_tail(&mut out);
    out
}

fn push_core_metadata_attr(out: &mut String, core_metadata: &CoreMetadata) {
    match core_metadata {
        CoreMetadata::Absent => {}
        CoreMetadata::Available => out.push_str(" data-core-metadata=\"true\" data-dist-info-metadata=\"true\""),
        CoreMetadata::Hashes(hashes) => match hashes.get("sha256") {
            Some(sha256) => {
                let _ = write!(
                    out,
                    " data-core-metadata=\"sha256={sha256}\" data-dist-info-metadata=\"sha256={sha256}\""
                );
            }
            None => out.push_str(" data-core-metadata=\"true\" data-dist-info-metadata=\"true\""),
        },
    }
}

fn push_head(out: &mut String, title: &str, meta: &Meta) {
    out.push_str("<!DOCTYPE html>\n<html>\n  <head>\n");
    push_meta(
        out,
        "    <meta name=\"pypi:repository-version\" content=\"",
        meta.api_version,
    );
    if let Some(status) = &meta.project_status {
        push_meta(out, "    <meta name=\"pypi:project-status\" content=\"", status);
    }
    if let Some(reason) = &meta.project_status_reason {
        push_meta(out, "    <meta name=\"pypi:project-status-reason\" content=\"", reason);
    }
    out.push_str("    <title>");
    push_escaped(out, title, Escape::Text);
    out.push_str("</title>\n");
    out.push_str("  </head>\n  <body>\n");
}

fn push_tail(out: &mut String) {
    out.push_str("  </body>\n</html>\n");
}

/// Open an attribute, escape its value into the page, and close the quote.
fn push_attr(out: &mut String, opening: &str, value: &str) {
    out.push_str(opening);
    push_escaped(out, value, Escape::Attr);
    out.push('"');
}

/// The same for a `<meta>` element, which closes with `">` and a newline.
fn push_meta(out: &mut String, opening: &str, value: &str) {
    push_attr(out, opening, value);
    out.push_str(">\n");
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    /// Element text, where a bare `"` is legal.
    Text,
    /// A double-quoted attribute value, where it is not.
    Attr,
}

/// Escape into the page buffer, copying each run between entities whole.
///
/// The escapees are all ASCII, so a byte index into `text` is always a char boundary. Rendering a
/// 400-file page calls this a few thousand times and almost nothing it is handed — a URL, a wheel
/// filename, `>=3.8` — needs escaping at all, so returning a `String` per field allocated one buffer
/// and copied it twice for nothing.
fn push_escaped(out: &mut String, text: &str, escape: Escape) {
    let mut run = 0;
    for (position, byte) in text.bytes().enumerate() {
        let entity = match byte {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' if escape == Escape::Attr => "&quot;",
            _ => continue,
        };
        out.push_str(&text[run..position]);
        out.push_str(entity);
        run = position + 1;
    }
    out.push_str(&text[run..]);
}
