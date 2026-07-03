//! Parse a PEP 503 HTML simple detail page into the model, so velodex can proxy HTML-only upstreams
//! (Artifactory, GitLab, plain static indexes) and re-serve them as JSON downstream.

use std::collections::BTreeMap;

use tl::{HTMLTag, ParserOptions};
use url::Url;

use super::simple::{CoreMetadata, File, ParsedDetail, Yanked};

/// Parse the HTML detail page for `project`, resolving relative file links against `base`.
///
/// A page that does not parse yields an empty file list rather than an error: a degraded upstream
/// should not take the whole request down.
#[must_use]
pub fn parse_detail_html(project: &str, html: &str, base: &Url) -> ParsedDetail {
    let files = tl::parse(html, ParserOptions::default())
        .map(|dom| {
            let parser = dom.parser();
            dom.query_selector("a")
                .into_iter()
                .flatten()
                .filter_map(|handle| handle.get(parser).and_then(|node| node.as_tag()))
                .filter_map(|tag| anchor_to_file(tag, tag.inner_text(parser).into_owned(), base))
                .collect()
        })
        .unwrap_or_default();
    ParsedDetail {
        name: project.to_owned(),
        versions: Vec::new(),
        files,
    }
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
        core_metadata: parse_core_metadata(tag),
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

fn parse_core_metadata(tag: &HTMLTag) -> CoreMetadata {
    let attrs = tag.attributes();
    let Some(present) = attrs
        .get("data-core-metadata")
        .or_else(|| attrs.get("data-dist-info-metadata"))
    else {
        return CoreMetadata::Absent;
    };
    let value = present.map(|value| value.as_utf8_str()).unwrap_or_default();
    match value.split_once('=') {
        Some((algo, hash)) => CoreMetadata::Hashes(BTreeMap::from([(algo.to_owned(), hash.to_owned())])),
        None => CoreMetadata::Available,
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
