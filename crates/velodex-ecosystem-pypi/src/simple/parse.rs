//! Parsing upstream PEP 691 JSON documents and the served response model.

use serde::{Deserialize, Serialize};

use super::meta::IncomingMeta;
use super::{File, Meta, SimpleError};

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
