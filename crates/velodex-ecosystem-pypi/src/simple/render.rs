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
        let normalized = crate::normalize_name_cow(&entry.name);
        let _ = writeln!(out, "    <a href=\"{normalized}/\">{}</a>", escape_text(&entry.name));
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
        out.push_str(&escape_attr(&file.url));
        if let Some(sha256) = file.hashes.get("sha256") {
            let _ = write!(out, "#sha256={sha256}");
        }
        out.push('"');
        if let Some(requires_python) = &file.requires_python {
            let _ = write!(out, " data-requires-python=\"{}\"", escape_attr(requires_python));
        }
        if let Some(gpg_sig) = file.gpg_sig {
            let _ = write!(out, " data-gpg-sig=\"{gpg_sig}\"");
        }
        match &file.yanked {
            Yanked::No => {}
            Yanked::Yes => out.push_str(" data-yanked=\"\""),
            Yanked::Reason(reason) => {
                let _ = write!(out, " data-yanked=\"{}\"", escape_attr(reason));
            }
        }
        if let Provenance::Url(url) = &file.provenance {
            let _ = write!(out, " data-provenance=\"{}\"", escape_attr(url));
        }
        push_core_metadata_attr(&mut out, file.metadata());
        let _ = writeln!(out, ">{}</a><br />", escape_text(&file.filename));
    }
    push_tail(&mut out);
    out
}

fn push_core_metadata_attr(out: &mut String, core_metadata: &CoreMetadata) {
    match core_metadata {
        CoreMetadata::Absent => {}
        CoreMetadata::Available => out.push_str(" data-core-metadata=\"true\" data-dist-info-metadata=\"true\""),
        CoreMetadata::Hashes(hashes) => {
            if let Some(sha256) = hashes.get("sha256") {
                let _ = write!(
                    out,
                    " data-core-metadata=\"sha256={sha256}\" data-dist-info-metadata=\"sha256={sha256}\""
                );
            }
        }
    }
}

fn push_head(out: &mut String, title: &str, meta: &Meta) {
    out.push_str("<!DOCTYPE html>\n<html>\n  <head>\n");
    let _ = writeln!(
        out,
        "    <meta name=\"pypi:repository-version\" content=\"{}\">",
        escape_attr(meta.api_version)
    );
    if let Some(status) = &meta.project_status {
        let _ = writeln!(
            out,
            "    <meta name=\"pypi:project-status\" content=\"{}\">",
            escape_attr(status)
        );
    }
    if let Some(reason) = &meta.project_status_reason {
        let _ = writeln!(
            out,
            "    <meta name=\"pypi:project-status-reason\" content=\"{}\">",
            escape_attr(reason)
        );
    }
    let _ = writeln!(out, "    <title>{}</title>", escape_text(title));
    out.push_str("  </head>\n  <body>\n");
}

fn push_tail(out: &mut String) {
    out.push_str("  </body>\n</html>\n");
}

fn escape_text(text: &str) -> String {
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

fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}
