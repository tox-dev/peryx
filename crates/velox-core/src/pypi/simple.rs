//! The PEP 503 / 691 simple repository API: data model and byte-exact serialization.
//!
//! velox precomputes these responses at index-update time and serves the bytes, so both the JSON
//! (PEP 691) and HTML (PEP 503) forms are produced here once from the same model.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Serialize, Serializer};

/// The Simple API version velox advertises. `1.1` covers PEP 700 (`versions`, `size`,
/// `upload-time`).
pub const API_VERSION: &str = "1.1";

/// The `meta` object shared by both response kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meta {
    #[serde(rename = "api-version")]
    pub api_version: &'static str,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            api_version: API_VERSION,
        }
    }
}

/// Whether a file is yanked (PEP 592): not yanked, yanked, or yanked with a reason.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Yanked {
    #[default]
    No,
    Yes,
    Reason(String),
}

impl Serialize for Yanked {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::No => serializer.serialize_bool(false),
            Self::Yes => serializer.serialize_bool(true),
            Self::Reason(reason) => serializer.serialize_str(reason),
        }
    }
}

/// Availability of the PEP 658/714 core-metadata sibling for a file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CoreMetadata {
    #[default]
    Absent,
    Available,
    Hashes(BTreeMap<String, String>),
}

impl Serialize for CoreMetadata {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent => serializer.serialize_bool(false),
            Self::Available => serializer.serialize_bool(true),
            Self::Hashes(hashes) => hashes.serialize(serializer),
        }
    }
}

/// One downloadable file in a project's detail page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct File {
    pub filename: String,
    pub url: String,
    pub hashes: BTreeMap<String, String>,
    #[serde(rename = "requires-python", skip_serializing_if = "Option::is_none")]
    pub requires_python: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(rename = "upload-time", skip_serializing_if = "Option::is_none")]
    pub upload_time: Option<String>,
    pub yanked: Yanked,
    #[serde(rename = "core-metadata")]
    pub core_metadata: CoreMetadata,
}

/// A project's detail response (`/simple/<project>/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectDetail {
    pub meta: Meta,
    pub name: String,
    pub versions: Vec<String>,
    pub files: Vec<File>,
}

/// One entry in the root project list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectListEntry {
    pub name: String,
}

/// The root project list (`/simple/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectList {
    pub meta: Meta,
    pub projects: Vec<ProjectListEntry>,
}

/// Serialize a value to PEP 691 JSON.
///
/// # Panics
/// Never in practice: the model contains only string-keyed maps and plain values, which
/// `serde_json` always serializes.
#[must_use]
pub fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("simple-API model always serializes to JSON")
}

/// Render the PEP 503 HTML for the root project list. The `href` is the normalized name; the
/// anchor text is the project's display name.
#[must_use]
pub fn render_index_html(list: &ProjectList) -> String {
    let mut out = String::new();
    push_head(&mut out, "Simple index");
    for entry in &list.projects {
        let normalized = crate::pypi::normalize_name(&entry.name);
        let _ = writeln!(out, "    <a href=\"{normalized}/\">{}</a>", escape_text(&entry.name));
    }
    push_tail(&mut out);
    out
}

/// Render the PEP 503 HTML for a project detail page.
#[must_use]
pub fn render_detail_html(detail: &ProjectDetail) -> String {
    let mut out = String::new();
    push_head(&mut out, &format!("Links for {}", detail.name));
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
        match &file.yanked {
            Yanked::No => {}
            Yanked::Yes => out.push_str(" data-yanked=\"\""),
            Yanked::Reason(reason) => {
                let _ = write!(out, " data-yanked=\"{}\"", escape_attr(reason));
            }
        }
        push_core_metadata_attr(&mut out, &file.core_metadata);
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

fn push_head(out: &mut String, title: &str) {
    out.push_str("<!DOCTYPE html>\n<html>\n  <head>\n");
    let _ = writeln!(
        out,
        "    <meta name=\"pypi:repository-version\" content=\"{API_VERSION}\">"
    );
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
