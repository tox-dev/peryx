//! Parsing upstream PEP 691 JSON documents and the served response model.

use std::fmt;
use std::io::Read;

use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use url::Url;

use super::meta::IncomingMeta;
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
/// upstream advertises a Simple API major version peryx does not support.
pub fn parse_detail(bytes: &[u8]) -> Result<ParsedDetail, SimpleError> {
    let detail: IncomingDetail = serde_json::from_slice(bytes)?;
    Ok(ParsedDetail {
        meta: detail.meta.into_meta()?,
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
    let mut header = DetailHeader::default();
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    DetailSeed {
        base,
        sink,
        header: &mut header,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(StreamedDetail {
        meta: header.meta.into_meta()?,
        name: header.name.unwrap_or_default(),
        versions: header.versions,
    })
}

#[derive(Default)]
struct DetailHeader {
    meta: IncomingMeta,
    name: Option<String>,
    versions: Vec<String>,
}

struct DetailSeed<'a, S: DetailSink> {
    base: &'a Url,
    sink: &'a mut S,
    header: &'a mut DetailHeader,
}

impl<'de, S: DetailSink> DeserializeSeed<'de> for DetailSeed<'_, S> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(DetailVisitor {
            base: self.base,
            sink: self.sink,
            header: self.header,
        })
    }
}

struct DetailVisitor<'a, S: DetailSink> {
    base: &'a Url,
    sink: &'a mut S,
    header: &'a mut DetailHeader,
}

impl<'de, S: DetailSink> Visitor<'de> for DetailVisitor<'_, S> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a PEP 691 project detail object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "meta" => self.header.meta = map.next_value()?,
                "name" => self.header.name = Some(map.next_value()?),
                "versions" => self.header.versions = map.next_value()?,
                "files" => map.next_value_seed(FilesSeed {
                    base: self.base,
                    sink: self.sink,
                })?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
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
/// Panics if serialization of the PEP 691 model fails.
#[must_use]
pub fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("simple-API model always serializes to JSON")
}
