//! Core-metadata parsing: the `METADATA` document of a wheel (also served as the PEP 658 sibling),
//! RFC 822-style headers followed by an optional long-description body.

use std::collections::HashMap;

use crate::distribution_version_segment;
use crate::version::{VersionKey, version_key};

/// The fields of a core-metadata document that the web UI presents, in the spirit of a pypi.org
/// project page. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct CoreMetadataDoc {
    pub metadata_version: Option<String>,
    pub name: String,
    pub version: String,
    pub summary: Option<String>,
    pub requires_python: Option<String>,
    pub license: Option<String>,
    pub license_expression: Option<String>,
    pub license_files: Vec<String>,
    pub author: Option<String>,
    pub maintainer: Option<String>,
    pub keywords: Vec<String>,
    pub requires_dist: Vec<String>,
    pub provides_dist: Vec<String>,
    pub obsoletes_dist: Vec<String>,
    pub provides_extra: Vec<String>,
    pub classifiers: Vec<String>,
    /// `(label, url)` pairs from `Project-URL` headers.
    pub project_urls: Vec<(String, String)>,
    pub home_page: Option<String>,
    pub download_url: Option<String>,
    /// The long description: the `Description` header or the document body.
    pub description: String,
    /// The `Description-Content-Type`, for example `text/markdown`.
    pub description_content_type: Option<String>,
}

/// Why a core-metadata document was rejected. Each variant is a defect `packaging` and Warehouse
/// both refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    /// A header line carries no colon. `email.parser` ends the header block there, so every field
    /// below it is lost.
    MissingHeaderSeparator(String),
    /// A header line starts with a colon, naming no field.
    MissingHeaderName(String),
    /// The document opens with a folded continuation line, which continues no header.
    LeadingContinuation(String),
    /// A single-use field appears more than once, leaving no one value to read. Holds the field name,
    /// lowercased.
    RepeatedField(String),
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHeaderSeparator(line) => write!(f, "header line {line:?} is missing a colon"),
            Self::MissingHeaderName(line) => write!(f, "header line {line:?} has no field name"),
            Self::LeadingContinuation(line) => {
                write!(f, "document starts with the continuation line {line:?}")
            }
            Self::RepeatedField(field) => write!(f, "single-use field {field:?} appears more than once"),
        }
    }
}

impl std::error::Error for MetadataError {}

/// The single-use core-metadata fields, lowercased. Every other field either repeats by specification
/// (`Classifier`, `Requires-Dist`, `Project-URL`, `License-File`, …) or is one core metadata does not
/// name; both may appear any number of times.
const SINGLE_USE_FIELDS: [&str; 16] = [
    "metadata-version",
    "name",
    "version",
    "summary",
    "description",
    "description-content-type",
    "keywords",
    "home-page",
    "download-url",
    "author",
    "author-email",
    "maintainer",
    "maintainer-email",
    "license",
    "license-expression",
    "requires-python",
];

/// Parse a core-metadata document.
///
/// # Errors
/// Returns [`MetadataError`] when the header block is not a well-formed RFC 822 message, or when a
/// single-use field repeats.
pub fn parse_metadata(text: &str) -> Result<CoreMetadataDoc, MetadataError> {
    let mut doc = CoreMetadataDoc::default();
    let (headers, body) = split_document(text);
    // A bit per SINGLE_USE_FIELDS entry: the repeat check allocates nothing.
    let mut seen: u16 = 0;
    for (key, raw) in unfold(headers)? {
        if let Some(index) = SINGLE_USE_FIELDS.iter().position(|field| *field == key) {
            let bit = 1 << index;
            if seen & bit != 0 {
                return Err(MetadataError::RepeatedField(key));
            }
            seen |= bit;
        }
        let value = raw.trim();
        match key.as_str() {
            "metadata-version" => doc.metadata_version = non_empty(value),
            "name" => value.clone_into(&mut doc.name),
            "version" => value.clone_into(&mut doc.version),
            "summary" => doc.summary = non_empty(value),
            "requires-python" => doc.requires_python = non_empty(value),
            "license" => doc.license = non_empty(value),
            "license-expression" => doc.license_expression = non_empty(value),
            "license-file" => doc.license_files.push(value.to_owned()),
            "author" | "author-email" => doc.author = doc.author.or_else(|| non_empty(value)),
            "maintainer" | "maintainer-email" => doc.maintainer = doc.maintainer.or_else(|| non_empty(value)),
            "keywords" => {
                doc.keywords = value
                    .split(',')
                    .map(str::trim)
                    .filter(|keyword| !keyword.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            "requires-dist" => doc.requires_dist.push(value.to_owned()),
            "provides-dist" => doc.provides_dist.push(value.to_owned()),
            "obsoletes-dist" => doc.obsoletes_dist.push(value.to_owned()),
            "provides-extra" => doc.provides_extra.push(value.to_owned()),
            "classifier" => doc.classifiers.push(value.to_owned()),
            "project-url" => {
                let (label, url) = value.split_once(',').unwrap_or(("", value));
                doc.project_urls.push((label.trim().to_owned(), url.trim().to_owned()));
            }
            "home-page" => doc.home_page = non_empty(value),
            "download-url" => doc.download_url = non_empty(value),
            // Drop only the RFC 822 separator whitespace; the rest of the payload is the renderer's
            // to fold, so trimming its body would corrupt the description.
            "description" => raw.trim_start().clone_into(&mut doc.description),
            "description-content-type" => doc.description_content_type = non_empty(value),
            _ => {}
        }
    }
    // No description header: the body is the payload verbatim. Its leading indentation and trailing
    // whitespace carry meaning (a Markdown code block, say), so the renderer folds them, not us.
    if doc.description.is_empty() {
        body.clone_into(&mut doc.description);
    }
    Ok(doc)
}

/// Cut the document at its first empty line, the header/body boundary. A document mixes CRLF and LF
/// endings when its long description comes from a CRLF README, so the boundary is the first line
/// that is empty once its ending is stripped, not a fixed `\r\n\r\n` or `\n\n` byte pair.
fn split_document(text: &str) -> (&str, &str) {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            return (&text[..offset], &text[offset + line.len()..]);
        }
        offset += line.len();
    }
    (text, "")
}

/// Split the header block into lowercased `(field, value)` pairs, joining folded (indented
/// continuation) lines per RFC 822 folding.
fn unfold(headers: &str) -> Result<Vec<(String, String)>, MetadataError> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for raw in headers.lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            let Some((_, value)) = fields.last_mut() else {
                return Err(MetadataError::LeadingContinuation(raw.to_owned()));
            };
            value.push(' ');
            value.push_str(raw.trim_start());
            continue;
        }
        let Some((key, value)) = raw.split_once(':') else {
            return Err(MetadataError::MissingHeaderSeparator(raw.to_owned()));
        };
        if key.is_empty() {
            return Err(MetadataError::MissingHeaderName(raw.to_owned()));
        }
        fields.push((key.to_ascii_lowercase(), value.to_owned()));
    }
    Ok(fields)
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

impl CoreMetadataDoc {
    /// Turn this `PyPI` core-metadata document into the neutral view model the web UI renders, mapping
    /// each header to a metadata-panel block the way a pypi.org project page presents it.
    #[must_use]
    pub fn to_ui_meta(&self) -> peryx_core::UiMeta {
        use peryx_core::{UiBlock, UiMeta};

        let mut blocks = Vec::new();
        for (label, value) in [
            ("Requires Python", self.requires_python.as_ref()),
            ("License", self.license_expression.as_ref().or(self.license.as_ref())),
            ("Author", self.author.as_ref()),
            ("Maintainer", self.maintainer.as_ref()),
        ] {
            if let Some(value) = value {
                blocks.push(UiBlock::KeyValue {
                    label: label.to_owned(),
                    value: value.clone(),
                });
            }
        }
        for (label, values) in [("Keywords", &self.keywords), ("Dependencies", &self.requires_dist)] {
            if !values.is_empty() {
                blocks.push(UiBlock::Chips {
                    label: label.to_owned(),
                    values: values.clone(),
                });
            }
        }
        let mut links: Vec<(String, String)> = self
            .project_urls
            .iter()
            .map(|(label, url)| {
                let label = well_known_label(label).map_or_else(|| label.clone(), str::to_owned);
                (label, url.clone())
            })
            .collect();
        if let Some(home_page) = &self.home_page {
            links.push(("Homepage".to_owned(), home_page.clone()));
        }
        if !links.is_empty() {
            blocks.push(UiBlock::Links {
                label: "Links".to_owned(),
                links,
            });
        }
        if let Some(groups) = classifier_groups(&self.classifiers) {
            blocks.push(UiBlock::Groups {
                label: "Classifiers".to_owned(),
                groups,
            });
        }
        UiMeta {
            version: non_empty(&self.version),
            summary: self.summary.clone(),
            description: self.render_description(),
            blocks,
        }
    }

    /// The description rendered to safe HTML, done here so the browser never runs the renderer. Only
    /// the serving build carries the renderer; the format-core build leaves it unrendered.
    fn render_description(&self) -> Option<peryx_core::RenderedDescription> {
        #[cfg(feature = "serving")]
        {
            non_empty(&self.description)
                .map(|text| crate::description::render(&text, self.description_content_type.as_deref()))
        }
        #[cfg(not(feature = "serving"))]
        {
            None
        }
    }
}

/// The display name of a `Project-URL` label under the well-known project URLs specification (PEP 753),
/// or `None` for a label that is not well known and so is presented as published. Labels match after
/// their ASCII punctuation and whitespace are deleted and the rest is lowercased, so `Bug Tracker`,
/// `bug_tracker` and `bugtracker` all render as `Issue Tracker`.
fn well_known_label(label: &str) -> Option<&'static str> {
    let normalized = label
        .chars()
        .filter(|character| !character.is_ascii_punctuation() && !character.is_ascii_whitespace())
        .collect::<String>()
        .to_lowercase();
    Some(match normalized.as_str() {
        "homepage" => "Homepage",
        "source" | "repository" | "sourcecode" | "github" => "Source Code",
        "download" => "Download",
        "changelog" | "changes" | "whatsnew" | "history" => "Changelog",
        "releasenotes" => "Release Notes",
        "documentation" | "docs" => "Documentation",
        "issues" | "issue" | "bugs" | "tracker" | "issuetracker" | "bugtracker" => "Issue Tracker",
        "funding" | "sponsor" | "donate" | "donation" => "Funding",
        _ => return None,
    })
}

/// Group trove classifiers by their top-level `::`-separated category, the way pypi.org presents them.
/// `None` when there are none, so the caller emits no block.
fn classifier_groups(classifiers: &[String]) -> Option<Vec<(String, Vec<String>)>> {
    if classifiers.is_empty() {
        return None;
    }
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for classifier in classifiers {
        let (group, rest) = classifier
            .split_once(" :: ")
            .map_or((classifier.as_str(), classifier.as_str()), |(g, r)| (g, r));
        match groups.iter_mut().find(|(name, _)| name == group) {
            Some((_, values)) => values.push(rest.to_owned()),
            None => groups.push((group.to_owned(), vec![rest.to_owned()])),
        }
    }
    Some(groups)
}

/// Parse a `PyPI` core-metadata document straight into the neutral [`UiMeta`](peryx_core::UiMeta) the
/// web UI renders.
///
/// # Errors
/// Returns [`MetadataError`] when the document's header block is malformed.
pub fn ui_meta(metadata_text: &str) -> Result<peryx_core::UiMeta, MetadataError> {
    Ok(parse_metadata(metadata_text)?.to_ui_meta())
}

/// Build a neutral [`UiProject`](peryx_core::UiProject) from a PEP 691 project-detail JSON document.
///
/// This is the shape the web project page renders. The `PyPI`-specific field names (`core-metadata`,
/// PEP 592 `yanked`) are read here so the UI never sees them.
#[must_use]
pub fn ui_project_from_detail(value: &serde_json::Value) -> peryx_core::UiProject {
    fn string_at(value: &serde_json::Value, key: &str) -> String {
        value[key].as_str().unwrap_or_default().to_owned()
    }
    let files = value["files"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| peryx_core::UiFile {
            filename: string_at(file, "filename"),
            url: string_at(file, "url"),
            sha256: file["hashes"]["sha256"].as_str().unwrap_or_default().to_owned(),
            size: file["size"].as_u64(),
            upload_time: file["upload-time"].as_str().map(str::to_owned),
            yanked: file_yanked(file),
            yanked_reason: file["yanked"]
                .as_str()
                .filter(|reason| !reason.is_empty())
                .map(str::to_owned),
            has_metadata: file["core-metadata"].is_object() || file["core-metadata"].as_bool() == Some(true),
        })
        .collect();
    peryx_core::UiProject {
        name: string_at(value, "name"),
        versions: releases(value),
        files,
    }
}

/// PEP 592 spells a yank as `true` or as the reason itself, so a string counts as a yank too.
fn file_yanked(file: &serde_json::Value) -> bool {
    file["yanked"].as_bool().unwrap_or(false) || file["yanked"].is_string()
}

/// The releases the detail page declares, each with the yank state its files give it.
///
/// A release is yanked when the publisher yanked every file of the PEP 440-equivalent version, so one
/// active file keeps the release active and a release with no files is not yanked. Its reasons are the
/// distinct nonempty ones its files carry, in the order the index lists them.
fn releases(value: &serde_json::Value) -> Vec<peryx_core::UiRelease> {
    let mut yanks: HashMap<VersionKey, ReleaseYank> = HashMap::new();
    for file in value["files"].as_array().into_iter().flatten() {
        let Some(version) = file["filename"].as_str().and_then(distribution_version_segment) else {
            continue;
        };
        let yank = yanks.entry(version_key(version)).or_default();
        if !file_yanked(file) {
            yank.active = true;
        } else if let Some(reason) = file["yanked"].as_str().filter(|reason| !reason.is_empty())
            && !yank.reasons.iter().any(|seen| seen == reason)
        {
            yank.reasons.push(reason.to_owned());
        }
    }
    value["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(|version| {
            let yank = yanks.get(&version_key(version)).filter(|yank| !yank.active);
            peryx_core::UiRelease {
                version: version.to_owned(),
                yanked: yank.is_some(),
                yanked_reasons: yank.map(|yank| yank.reasons.clone()).unwrap_or_default(),
            }
        })
        .collect()
}

#[derive(Default)]
struct ReleaseYank {
    /// Whether any file of the release is still usable, which keeps the whole release active.
    active: bool,
    reasons: Vec<String>,
}
