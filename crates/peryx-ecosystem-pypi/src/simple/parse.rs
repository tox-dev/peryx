//! Parsing upstream PEP 691 JSON documents and the served response model.

use std::fmt;
use std::io::Read;

use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Serialize};
use url::Url;

use super::meta::{IncomingMeta, IncomingProjectStatus};
use super::{File, Meta, SimpleError};

/// Resolve `url` in place against `base`, turning a relative, root-relative, or protocol-relative
/// PEP 691 file reference into an absolute URL. An already-absolute URL is left byte-for-byte intact.
///
/// `PyPI` proper serves absolute URLs, but a static index (`dumb-pypi`, GitLab, Artifactory) may not;
/// peryx must content-address and re-serve those files, which needs an absolute source URL.
pub fn absolutize(base: &Url, url: &mut String) {
    if Url::parse(url).is_ok() {
        return;
    }
    if let Ok(resolved) = base.join(url) {
        *url = resolved.into();
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
    meta: IncomingMeta,
    #[serde(rename = "project-status", default, deserialize_with = "deserialize_project_status")]
    project_status: Option<IncomingProjectStatus>,
    name: String,
    #[serde(default)]
    versions: Vec<String>,
    files: Vec<File>,
}

/// Parse an upstream PEP 691 JSON project detail.
///
/// # Errors
/// Returns an error when `bytes` is not a valid PEP 691 project detail document, or when the
/// upstream advertises a Simple API major version peryx does not support.
pub fn parse_detail(bytes: &[u8]) -> Result<ParsedDetail, SimpleError> {
    let detail: IncomingDetail = serde_json::from_slice(bytes)?;
    Ok(ParsedDetail {
        meta: detail.meta.into_detail_meta(detail.project_status)?,
        name: detail.name,
        versions: detail.versions,
        files: detail.files,
    })
}

/// A receiver for files decoded during a streaming detail parse.
///
/// The parser hands each file over as soon as it is read, so the sink can apply policy and flush
/// bounded batches to storage without the whole (potentially million-file) document ever living in
/// memory at once.
pub trait DetailSink {
    /// The sink's own failure, surfaced through the parse as a rejected document.
    type Error: fmt::Display;

    /// # Errors
    /// Returns the sink's error when the file cannot be accepted, which aborts the parse.
    fn file(&mut self, file: File) -> Result<(), Self::Error>;
}

/// The header fields a streamed detail carries alongside its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedDetail {
    pub meta: Meta,
    pub name: String,
    pub versions: Vec<String>,
}

/// Stream-parse a PEP 691 JSON project detail, resolving each file URL against `base` and handing it
/// to `sink` as it is decoded. The header (`meta`, `name`, `versions`) returns once the files drain.
///
/// # Errors
/// Returns [`SimpleError`] when the body is not a valid PEP 691 detail, advertises an unsupported
/// Simple API version, or the sink rejects a file.
pub fn stream_detail_json<S: DetailSink>(
    reader: impl Read,
    base: &Url,
    sink: &mut S,
) -> Result<StreamedDetail, SimpleError> {
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let header = DetailSeed { base, sink }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(StreamedDetail {
        meta: header.meta.into_detail_meta(header.project_status)?,
        name: header.name,
        versions: header.versions,
    })
}

struct IncomingStreamedDetail {
    meta: IncomingMeta,
    project_status: Option<IncomingProjectStatus>,
    name: String,
    versions: Vec<String>,
}

struct DetailSeed<'a, S: DetailSink> {
    base: &'a Url,
    sink: &'a mut S,
}

impl<'de, S: DetailSink> DeserializeSeed<'de> for DetailSeed<'_, S> {
    type Value = IncomingStreamedDetail;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(DetailVisitor {
            base: self.base,
            sink: self.sink,
        })
    }
}

struct DetailVisitor<'a, S: DetailSink> {
    base: &'a Url,
    sink: &'a mut S,
}

impl<'de, S: DetailSink> Visitor<'de> for DetailVisitor<'_, S> {
    type Value = IncomingStreamedDetail;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a PEP 691 project detail object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut meta = None;
        let mut project_status = None;
        let mut name = None;
        let mut versions = Vec::new();
        let mut files = false;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "meta" => meta = Some(map.next_value()?),
                "project-status" => project_status = Some(map.next_value()?),
                "name" => name = Some(map.next_value()?),
                "versions" => versions = map.next_value()?,
                "files" => {
                    map.next_value_seed(FilesSeed {
                        base: self.base,
                        sink: self.sink,
                    })?;
                    files = true;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        if !files {
            return Err(serde::de::Error::missing_field("files"));
        }
        Ok(IncomingStreamedDetail {
            meta: meta.ok_or_else(|| serde::de::Error::missing_field("meta"))?,
            project_status,
            name: name.ok_or_else(|| serde::de::Error::missing_field("name"))?,
            versions,
        })
    }
}

struct FilesSeed<'a, S: DetailSink> {
    base: &'a Url,
    sink: &'a mut S,
}

impl<'de, S: DetailSink> DeserializeSeed<'de> for FilesSeed<'_, S> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(FilesVisitor {
            base: self.base,
            sink: self.sink,
        })
    }
}

struct FilesVisitor<'a, S: DetailSink> {
    base: &'a Url,
    sink: &'a mut S,
}

impl<'de, S: DetailSink> Visitor<'de> for FilesVisitor<'_, S> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a file array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        while let Some(mut file) = sequence.next_element::<File>()? {
            absolutize(self.base, &mut file.url);
            file.provenance.retain_secure_url();
            self.sink.file(file).map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

/// Parse only an upstream Simple API `meta` object.
///
/// # Errors
/// Returns an error when the metadata is not valid JSON or advertises an unsupported API version.
pub fn parse_meta(bytes: &[u8]) -> Result<Meta, SimpleError> {
    let meta: IncomingMeta = serde_json::from_slice(bytes)?;
    meta.into_meta()
}

#[cfg(feature = "serving")]
pub fn parse_project_status(bytes: &[u8]) -> Result<(Option<String>, Option<String>), SimpleError> {
    let status: IncomingProjectStatus = serde_json::from_slice(bytes)?;
    let (status, reason) = status.into_parts();
    if let Some(status) = status.as_deref() {
        super::meta::validate_project_status(status)?;
    }
    Ok((status, reason))
}

fn deserialize_project_status<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<IncomingProjectStatus>, D::Error> {
    IncomingProjectStatus::deserialize(deserializer).map(Some)
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
/// upstream advertises a Simple API major version peryx does not support.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDetail {
    pub meta: Meta,
    pub name: String,
    pub versions: Vec<String>,
    pub files: Vec<File>,
}

impl Serialize for ProjectDetail {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let project_status = self.meta.project_status_object();
        let mut map = serializer.serialize_map(Some(4 + usize::from(project_status.is_some())))?;
        map.serialize_entry("meta", &self.meta)?;
        if let Some(project_status) = project_status {
            map.serialize_entry("project-status", &project_status)?;
        }
        map.serialize_entry("name", &self.name)?;
        map.serialize_entry("versions", &self.versions)?;
        map.serialize_entry("files", &self.files)?;
        map.end()
    }
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
/// Panics if serialization of the PEP 691 model fails.
#[must_use]
pub fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("simple-API model always serializes to JSON")
}
