//! The PEP 503 / 691 simple repository API: data model and byte-exact serialization.
//!
//! velodex precomputes these responses at index-update time and serves the bytes, so both the JSON
//! (PEP 691) and HTML (PEP 503) forms are produced here once from the same model.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The Simple API version velodex advertises.
pub const API_VERSION: &str = "1.4";
const API_MAJOR: u64 = 1;

/// An upstream Simple API document could not be used.
#[derive(Debug)]
pub enum SimpleError {
    /// The document was not valid JSON for the Simple API model.
    Json(serde_json::Error),
    /// The document was too large for the HTML parser.
    Html(tl::ParseError),
    /// The upstream advertised a backwards-incompatible Simple API major version.
    UnsupportedApiVersion(String),
    /// The upstream advertised a malformed Simple API version.
    InvalidApiVersion(String),
    /// The upstream advertised an unknown project status marker.
    InvalidProjectStatus(String),
}

impl fmt::Display for SimpleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => err.fmt(f),
            Self::Html(err) => write!(f, "invalid upstream Simple API HTML: {err}"),
            Self::UnsupportedApiVersion(version) => write!(
                f,
                "unsupported upstream Simple API version {version:?}; velodex supports Simple API 1.x"
            ),
            Self::InvalidApiVersion(version) => {
                write!(
                    f,
                    "invalid upstream Simple API version {version:?}; expected Major.Minor"
                )
            }
            Self::InvalidProjectStatus(status) => {
                write!(f, "invalid upstream project status marker {status:?}")
            }
        }
    }
}

impl std::error::Error for SimpleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            Self::Html(err) => Some(err),
            Self::UnsupportedApiVersion(_) | Self::InvalidApiVersion(_) | Self::InvalidProjectStatus(_) => None,
        }
    }
}

impl From<serde_json::Error> for SimpleError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<tl::ParseError> for SimpleError {
    fn from(err: tl::ParseError) -> Self {
        Self::Html(err)
    }
}

/// The `meta` object shared by both response kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meta {
    #[serde(rename = "api-version")]
    pub api_version: &'static str,
    #[serde(rename = "project-status", skip_serializing_if = "Option::is_none")]
    pub project_status: Option<String>,
    #[serde(rename = "project-status-reason", skip_serializing_if = "Option::is_none")]
    pub project_status_reason: Option<String>,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            api_version: API_VERSION,
            project_status: None,
            project_status_reason: None,
        }
    }
}

impl Meta {
    /// Build served metadata from upstream metadata after checking Simple API compatibility.
    ///
    /// # Errors
    /// Returns [`SimpleError`] when the upstream advertises an invalid or unsupported API version.
    pub fn from_upstream(
        api_version: Option<&str>,
        project_status: Option<String>,
        project_status_reason: Option<String>,
    ) -> Result<Self, SimpleError> {
        validate_api_version(api_version)?;
        if let Some(status) = project_status.as_deref() {
            validate_project_status(status)?;
        }
        Ok(Self {
            api_version: API_VERSION,
            project_status,
            project_status_reason,
        })
    }

    #[must_use]
    pub fn status(&self) -> ProjectStatus {
        self.project_status
            .as_deref()
            .and_then(ProjectStatus::from_marker)
            .unwrap_or(ProjectStatus::Active)
    }
}

#[derive(Default, Deserialize)]
struct IncomingMeta {
    #[serde(rename = "api-version", default)]
    api_version: Option<String>,
    #[serde(rename = "project-status", default)]
    project_status: Option<String>,
    #[serde(rename = "project-status-reason", default)]
    project_status_reason: Option<String>,
}

impl IncomingMeta {
    fn into_meta(self) -> Result<Meta, SimpleError> {
        Meta::from_upstream(
            self.api_version.as_deref(),
            self.project_status,
            self.project_status_reason,
        )
    }
}

fn validate_api_version(version: Option<&str>) -> Result<(), SimpleError> {
    let Some(version) = version else {
        return Ok(());
    };
    let Some((major, minor)) = version.split_once('.') else {
        return Err(SimpleError::InvalidApiVersion(version.to_owned()));
    };
    let major = major
        .parse::<u64>()
        .map_err(|_| SimpleError::InvalidApiVersion(version.to_owned()))?;
    if major != API_MAJOR {
        return Err(SimpleError::UnsupportedApiVersion(version.to_owned()));
    }
    minor
        .parse::<u64>()
        .map_err(|_| SimpleError::InvalidApiVersion(version.to_owned()))?;
    Ok(())
}

fn validate_project_status(status: &str) -> Result<(), SimpleError> {
    ProjectStatus::from_marker(status)
        .map(|_| ())
        .ok_or_else(|| SimpleError::InvalidProjectStatus(status.to_owned()))
}

/// The standardized project status markers and their serving policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    Active,
    Archived,
    Quarantined,
    Deprecated,
}

impl ProjectStatus {
    #[must_use]
    pub fn from_marker(status: &str) -> Option<Self> {
        match status {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            "quarantined" => Some(Self::Quarantined),
            "deprecated" => Some(Self::Deprecated),
            _ => None,
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Quarantined => "quarantined",
            Self::Deprecated => "deprecated",
        }
    }

    #[must_use]
    pub const fn allows_uploads(self) -> bool {
        matches!(self, Self::Active | Self::Deprecated)
    }

    #[must_use]
    pub const fn offers_downloads(self) -> bool {
        !matches!(self, Self::Quarantined)
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

impl<'de> Deserialize<'de> for Yanked {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct YankedVisitor;
        impl Visitor<'_> for YankedVisitor {
            type Value = Yanked;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a boolean or a reason string")
            }
            fn visit_bool<E>(self, value: bool) -> Result<Yanked, E> {
                Ok(if value { Yanked::Yes } else { Yanked::No })
            }
            fn visit_str<E>(self, value: &str) -> Result<Yanked, E> {
                Ok(Yanked::Reason(value.to_owned()))
            }
        }
        deserializer.deserialize_any(YankedVisitor)
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

impl CoreMetadata {
    /// Whether the file does not advertise a core-metadata sibling.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
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

impl<'de> Deserialize<'de> for CoreMetadata {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CoreMetadataVisitor;
        impl<'de> Visitor<'de> for CoreMetadataVisitor {
            type Value = CoreMetadata;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a boolean or a hashes object")
            }
            fn visit_bool<E>(self, value: bool) -> Result<CoreMetadata, E> {
                Ok(if value {
                    CoreMetadata::Available
                } else {
                    CoreMetadata::Absent
                })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<CoreMetadata, A::Error> {
                let mut hashes = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    hashes.insert(key, value);
                }
                Ok(CoreMetadata::Hashes(hashes))
            }
        }
        deserializer.deserialize_any(CoreMetadataVisitor)
    }
}

/// A file provenance URL from PEP 740, or an explicit JSON `null`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Provenance {
    #[default]
    Absent,
    None,
    Url(String),
}

impl<'de> Deserialize<'de> for Provenance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Option::<String>::deserialize(deserializer)?.map_or(Self::None, Self::Url))
    }
}

/// One downloadable file in a project's detail page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub filename: String,
    pub url: String,
    pub hashes: BTreeMap<String, String>,
    pub requires_python: Option<String>,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
    pub yanked: Yanked,
    pub core_metadata: CoreMetadata,
    pub dist_info_metadata: CoreMetadata,
    pub gpg_sig: Option<bool>,
    pub provenance: Provenance,
}

impl File {
    /// The effective metadata sibling advertised by either spelling, preferring the current key.
    #[must_use]
    pub const fn metadata(&self) -> &CoreMetadata {
        if self.core_metadata.is_absent() {
            &self.dist_info_metadata
        } else {
            &self.core_metadata
        }
    }

    /// Clear both metadata spellings after velodex cannot verify the sibling digest.
    pub fn clear_metadata(&mut self) {
        self.core_metadata = CoreMetadata::Absent;
        self.dist_info_metadata = CoreMetadata::Absent;
    }

    /// Set both metadata spellings for locally extracted metadata.
    pub fn set_metadata(&mut self, metadata: CoreMetadata) {
        self.core_metadata = metadata.clone();
        self.dist_info_metadata = metadata;
    }
}

#[derive(Deserialize)]
struct IncomingFile {
    filename: String,
    url: String,
    #[serde(default)]
    hashes: BTreeMap<String, String>,
    #[serde(rename = "requires-python", default)]
    requires_python: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(rename = "upload-time", default)]
    upload_time: Option<String>,
    #[serde(default)]
    yanked: Yanked,
    #[serde(rename = "core-metadata", default)]
    core_metadata: CoreMetadata,
    #[serde(rename = "dist-info-metadata", default)]
    dist_info_metadata: CoreMetadata,
    #[serde(rename = "gpg-sig", default)]
    gpg_sig: Option<bool>,
    #[serde(default)]
    provenance: Provenance,
}

impl From<IncomingFile> for File {
    fn from(file: IncomingFile) -> Self {
        Self {
            filename: file.filename,
            url: file.url,
            hashes: file.hashes,
            requires_python: file.requires_python,
            size: file.size,
            upload_time: file.upload_time,
            yanked: file.yanked,
            core_metadata: file.core_metadata,
            dist_info_metadata: file.dist_info_metadata,
            gpg_sig: file.gpg_sig,
            provenance: file.provenance,
        }
    }
}

impl<'de> Deserialize<'de> for File {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        IncomingFile::deserialize(deserializer).map(Self::from)
    }
}

impl Serialize for File {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("filename", &self.filename)?;
        map.serialize_entry("url", &self.url)?;
        map.serialize_entry("hashes", &self.hashes)?;
        if let Some(requires_python) = &self.requires_python {
            map.serialize_entry("requires-python", requires_python)?;
        }
        if let Some(size) = self.size {
            map.serialize_entry("size", &size)?;
        }
        if let Some(upload_time) = &self.upload_time {
            map.serialize_entry("upload-time", upload_time)?;
        }
        map.serialize_entry("yanked", &self.yanked)?;
        let metadata = self.metadata();
        map.serialize_entry("core-metadata", metadata)?;
        if !metadata.is_absent() {
            map.serialize_entry("dist-info-metadata", metadata)?;
        }
        if let Some(gpg_sig) = self.gpg_sig {
            map.serialize_entry("gpg-sig", &gpg_sig)?;
        }
        match &self.provenance {
            Provenance::Absent => {}
            Provenance::None => map.serialize_entry("provenance", &Option::<String>::None)?,
            Provenance::Url(url) => map.serialize_entry("provenance", url)?,
        }
        map.end()
    }
}

/// A project detail parsed from an upstream PEP 691 JSON response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDetail {
    pub meta: Meta,
    pub name: String,
    pub versions: Vec<String>,
    pub files: Vec<File>,
}

#[derive(Deserialize)]
struct IncomingDetail {
    #[serde(default)]
    meta: IncomingMeta,
    name: String,
    #[serde(default)]
    versions: Vec<String>,
    #[serde(default)]
    files: Vec<File>,
}

/// Parse an upstream PEP 691 JSON project detail.
///
/// # Errors
/// Returns an error when `bytes` is not a valid PEP 691 project detail document, or when the
/// upstream advertises a Simple API major version velodex does not support.
pub fn parse_detail(bytes: &[u8]) -> Result<ParsedDetail, SimpleError> {
    let detail: IncomingDetail = serde_json::from_slice(bytes)?;
    Ok(ParsedDetail {
        meta: detail.meta.into_meta()?,
        name: detail.name,
        versions: detail.versions,
        files: detail.files,
    })
}

/// Parse only an upstream Simple API `meta` object.
///
/// # Errors
/// Returns an error when the metadata is not valid JSON or advertises an unsupported API version.
pub fn parse_meta(bytes: &[u8]) -> Result<Meta, SimpleError> {
    let meta: IncomingMeta = serde_json::from_slice(bytes)?;
    meta.into_meta()
}

#[derive(Deserialize)]
struct IncomingProjectListEntry {
    name: String,
}

#[derive(Deserialize)]
struct IncomingProjectList {
    #[serde(default)]
    meta: IncomingMeta,
    #[serde(default)]
    projects: Vec<IncomingProjectListEntry>,
}

/// Parse an upstream PEP 691 JSON root project list.
///
/// # Errors
/// Returns an error when `bytes` is not a valid PEP 691 project list document, or when the
/// upstream advertises a Simple API major version velodex does not support.
pub fn parse_index(bytes: &[u8]) -> Result<ProjectList, SimpleError> {
    let list: IncomingProjectList = serde_json::from_slice(bytes)?;
    Ok(ProjectList {
        meta: list.meta.into_meta()?,
        projects: list
            .projects
            .into_iter()
            .map(|entry| ProjectListEntry { name: entry.name })
            .collect(),
    })
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
    push_head(&mut out, "Simple index", &list.meta);
    for entry in &list.projects {
        let normalized = crate::normalize_name(&entry.name);
        let _ = writeln!(out, "    <a href=\"{normalized}/\">{}</a>", escape_text(&entry.name));
    }
    push_tail(&mut out);
    out
}

/// Render the PEP 503 HTML for a project detail page.
#[must_use]
pub fn render_detail_html(detail: &ProjectDetail) -> String {
    let mut out = String::new();
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
