//! Parse a PEP 503 HTML simple detail page into the model, so velodex can proxy HTML-only upstreams
//! (Artifactory, GitLab, plain static indexes) and re-serve them as JSON downstream.

use std::collections::BTreeMap;

use tl::{HTMLTag, ParserOptions};
use url::Url;

use super::simple::{CoreMetadata, File, Meta, ParsedDetail, Provenance, SimpleError, Yanked};

/// Parse the HTML detail page for `project`, resolving relative file links against `base`.
///
/// # Errors
/// Returns an error when the HTML page advertises an unsupported Simple API major version.
pub fn parse_detail_html(project: &str, html: &str, base: &Url) -> Result<ParsedDetail, SimpleError> {
    let dom = tl::parse(html, ParserOptions::default())?;
    let parser = dom.parser();
    let meta = parse_meta(parser, &dom)?;
    let files = dom
        .query_selector("a")
        .into_iter()
        .flatten()
        .filter_map(|handle| handle.get(parser).and_then(|node| node.as_tag()))
        .filter_map(|tag| anchor_to_file(tag, tag.inner_text(parser).into_owned(), base))
        .collect();
    Ok(ParsedDetail {
        meta,
        name: project.to_owned(),
        versions: Vec::new(),
        files,
    })
}

fn parse_meta(parser: &tl::Parser, dom: &tl::VDom<'_>) -> Result<Meta, SimpleError> {
    let mut api_version = None;
    let mut project_status = None;
    let mut project_status_reason = None;
    for tag in dom
        .query_selector("meta")
        .into_iter()
        .flatten()
        .filter_map(|handle| handle.get(parser).and_then(|node| node.as_tag()))
    {
        let Some(name) = attr_string(tag, "name") else {
            continue;
        };
        match name.as_str() {
            "pypi:repository-version" => api_version = attr_string(tag, "content"),
            "pypi:project-status" => project_status = attr_string(tag, "content"),
            "pypi:project-status-reason" => project_status_reason = attr_string(tag, "content"),
            _ => {}
        }
    }
    Meta::from_upstream(api_version.as_deref(), project_status, project_status_reason)
}

fn anchor_to_file(tag: &HTMLTag, filename: String, base: &Url) -> Option<File> {
    let attrs = tag.attributes();
    let href = attrs.get("href").flatten()?.as_utf8_str();
    let mut resolved = base.join(&href).ok()?;
    let hashes = fragment_hash(resolved.fragment());
    resolved.set_fragment(None);
    Some(File {
        filename,
        url: resolved.to_string(),
        hashes,
        requires_python: attr_string(tag, "data-requires-python"),
        size: None,
        upload_time: None,
        yanked: parse_yanked(tag),
        core_metadata: parse_metadata_attr(tag, "data-core-metadata"),
        dist_info_metadata: parse_metadata_attr(tag, "data-dist-info-metadata"),
        gpg_sig: parse_gpg_sig(tag),
        provenance: attr_string(tag, "data-provenance").map_or(Provenance::Absent, Provenance::Url),
    })
}

fn fragment_hash(fragment: Option<&str>) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    if let Some((algo, value)) = fragment.and_then(|f| f.split_once('=')) {
        hashes.insert(algo.to_owned(), value.to_owned());
    }
    hashes
}

fn attr_string(tag: &HTMLTag, name: &str) -> Option<String> {
    tag.attributes()
        .get(name)
        .flatten()
        .map(|value| decode_entities(&value.as_utf8_str()))
}

fn parse_yanked(tag: &HTMLTag) -> Yanked {
    let Some(present) = tag.attributes().get("data-yanked") else {
        return Yanked::No;
    };
    let reason = present
        .map(|value| decode_entities(&value.as_utf8_str()))
        .unwrap_or_default();
    if reason.is_empty() {
        Yanked::Yes
    } else {
        Yanked::Reason(reason)
    }
}

fn parse_metadata_attr(tag: &HTMLTag, name: &str) -> CoreMetadata {
    let Some(present) = tag.attributes().get(name) else {
        return CoreMetadata::Absent;
    };
    let value = present.map(|value| value.as_utf8_str()).unwrap_or_default();
    match value.split_once('=') {
        Some((algo, hash)) => CoreMetadata::Hashes(BTreeMap::from([(algo.to_owned(), hash.to_owned())])),
        None => CoreMetadata::Available,
    }
}

fn parse_gpg_sig(tag: &HTMLTag) -> Option<bool> {
    let value = tag.attributes().get("data-gpg-sig")?;
    match value.map(|value| value.as_utf8_str()) {
        Some(value) if value.eq_ignore_ascii_case("false") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("true") => Some(true),
        None => Some(true),
        Some(_) => None,
    }
}

/// Decode the HTML entities PEP 503 attribute values may contain.
fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}
