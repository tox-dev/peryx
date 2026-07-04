//! Parse a PEP 503 HTML simple detail page into the model, so velodex can proxy HTML-only upstreams
//! (Artifactory, GitLab, plain static indexes) and re-serve them as JSON downstream.

use std::borrow::Cow;
use std::collections::BTreeMap;

use tl::{HTMLTag, ParserOptions};
use url::Url;

use super::simple::{
    CoreMetadata, File, Meta, ParsedDetail, ProjectList, ProjectListEntry, Provenance, SimpleError, Yanked,
};

/// Parse the HTML detail page for `project`, resolving relative file links against `base`.
///
/// # Errors
/// Returns an error when the HTML page advertises an unsupported Simple API major version.
pub fn parse_detail_html(project: &str, html: &str, base: &Url) -> Result<ParsedDetail, SimpleError> {
    let dom = tl::parse(html, ParserOptions::default())?;
    let meta = parse_meta(&dom)?;
    let base = link_base(&dom, base);
    let files = dom
        .nodes()
        .iter()
        .filter_map(|node| node.as_tag())
        .filter(|tag| is_tag(tag, b"a"))
        .filter_map(|tag| anchor_to_file(tag, &base))
        .collect();
    Ok(ParsedDetail {
        meta,
        name: project.to_owned(),
        versions: Vec::new(),
        files,
    })
}

/// Parse the HTML root project list, resolving anchors the same way a PEP 503 client would.
///
/// # Errors
/// Returns an error when the HTML page advertises an unsupported Simple API major version.
pub fn parse_index_html(html: &str, base: &Url) -> Result<ProjectList, SimpleError> {
    let dom = tl::parse(html, ParserOptions::default())?;
    let meta = parse_meta(&dom)?;
    let base = link_base(&dom, base);
    let projects = dom
        .nodes()
        .iter()
        .filter_map(|node| node.as_tag())
        .filter(|tag| is_tag(tag, b"a"))
        .filter_map(|tag| anchor_to_project(tag, &base, dom.parser()))
        .collect();
    Ok(ProjectList { meta, projects })
}

fn parse_meta(dom: &tl::VDom<'_>) -> Result<Meta, SimpleError> {
    let mut api_version = None;
    let mut project_status = None;
    let mut project_status_reason = None;
    for tag in dom
        .nodes()
        .iter()
        .filter_map(|node| node.as_tag())
        .filter(|tag| is_tag(tag, b"meta"))
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

fn link_base(dom: &tl::VDom<'_>, base: &Url) -> Url {
    dom.nodes()
        .iter()
        .filter_map(|node| node.as_tag())
        .take_while(|tag| !is_tag(tag, b"a") && !is_tag(tag, b"link"))
        .find(|tag| is_tag(tag, b"base"))
        .and_then(|tag| attr_string(tag, "href"))
        .and_then(|href| base.join(&href).ok())
        .unwrap_or_else(|| base.clone())
}

fn anchor_to_project(tag: &HTMLTag, base: &Url, parser: &tl::Parser<'_>) -> Option<ProjectListEntry> {
    let href = attr_string(tag, "href").filter(|href| !href.is_empty())?;
    let resolved = base.join(&href).ok()?;
    let name = tag.inner_text(parser);
    let name = decode_entities(name.trim());
    let name = if name.is_empty() {
        project_from_url(&resolved)?
    } else {
        name
    };
    Some(ProjectListEntry { name })
}

fn project_from_url(url: &Url) -> Option<String> {
    url.path()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(percent_decode)
}

fn anchor_to_file(tag: &HTMLTag, base: &Url) -> Option<File> {
    let href = attr_string(tag, "href").filter(|href| !href.is_empty())?;
    let mut resolved = base.join(&href).ok()?;
    let hashes = fragment_hash(resolved.fragment());
    let filename = filename_from_url(&resolved)?;
    resolved.set_fragment(None);
    Some(File {
        filename,
        url: resolved.to_string(),
        hashes,
        requires_python: attr_string(tag, "data-requires-python"),
        size: attr_string(tag, "data-size").and_then(|value| value.parse().ok()),
        upload_time: attr_string(tag, "data-upload-time"),
        yanked: parse_yanked(tag),
        core_metadata: parse_metadata_attr(tag, "data-core-metadata"),
        dist_info_metadata: parse_metadata_attr(tag, "data-dist-info-metadata"),
        gpg_sig: parse_gpg_sig(tag),
        provenance: attr_string(tag, "data-provenance").map_or(Provenance::Absent, Provenance::Url),
    })
}

fn filename_from_url(url: &Url) -> Option<String> {
    let filename = url
        .path()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|filename| !filename.is_empty())?;
    Some(percent_decode(filename))
}

fn fragment_hash(fragment: Option<&str>) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    let Some(fragment) = fragment.map(percent_decode) else {
        return hashes;
    };
    for part in fragment.split('&') {
        let Some((algo, value)) = part.split_once('=') else {
            continue;
        };
        if is_supported_hash(algo) && !value.is_empty() {
            hashes.insert(algo.to_owned(), value.to_owned());
            break;
        }
    }
    hashes
}

fn is_supported_hash(algo: &str) -> bool {
    matches!(algo, "sha512" | "sha384" | "sha256" | "sha224" | "sha1" | "md5")
}

fn is_tag(tag: &HTMLTag, name: &[u8]) -> bool {
    tag.name().as_bytes().eq_ignore_ascii_case(name)
}

fn attr_string(tag: &HTMLTag, name: &str) -> Option<String> {
    attr_value(tag, name).map(|value| decode_entities(&value))
}

fn attr_value<'tag>(tag: &'tag HTMLTag, name: &'tag str) -> Option<Cow<'tag, str>> {
    tag.attributes()
        .get(name)
        .flatten()
        .map(|value| value.as_utf8_str())
        .or_else(|| {
            tag.attributes().iter().find_map(|(attr_name, value)| {
                attr_name
                    .eq_ignore_ascii_case(name)
                    .then_some(value)
                    .and_then(|value| value)
            })
        })
}

fn has_attr(tag: &HTMLTag, name: &str) -> bool {
    tag.attributes().contains(name)
        || tag
            .attributes()
            .iter()
            .any(|(attr_name, _)| attr_name.eq_ignore_ascii_case(name))
}

fn parse_yanked(tag: &HTMLTag) -> Yanked {
    if !has_attr(tag, "data-yanked") {
        return Yanked::No;
    }
    let reason = attr_string(tag, "data-yanked").unwrap_or_default();
    if reason.is_empty() {
        Yanked::Yes
    } else {
        Yanked::Reason(reason)
    }
}

fn parse_metadata_attr(tag: &HTMLTag, name: &str) -> CoreMetadata {
    if !has_attr(tag, name) {
        return CoreMetadata::Absent;
    }
    let value = attr_string(tag, name).unwrap_or_default();
    match value.as_str() {
        "false" => CoreMetadata::Absent,
        "true" | "" => CoreMetadata::Available,
        _ => match value.split_once('=') {
            Some((algo, hash)) => CoreMetadata::Hashes(BTreeMap::from([(algo.to_owned(), hash.to_owned())])),
            None => CoreMetadata::Available,
        },
    }
}

fn parse_gpg_sig(tag: &HTMLTag) -> Option<bool> {
    if !has_attr(tag, "data-gpg-sig") {
        return None;
    }
    match attr_string(tag, "data-gpg-sig").as_deref() {
        Some(value) if value.eq_ignore_ascii_case("false") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("true") => Some(true),
        None | Some("") => Some(true),
        Some(_) => None,
    }
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn percent_decode(text: &str) -> String {
    let mut bytes = Vec::with_capacity(text.len());
    let input = text.as_bytes();
    let mut index = 0;
    while let Some(&byte) = input.get(index) {
        if byte == b'%'
            && let (Some(high), Some(low)) = (input.get(index + 1), input.get(index + 2))
            && let (Some(high), Some(low)) = (hex_value(*high), hex_value(*low))
        {
            bytes.push(high << 4 | low);
            index += 3;
        } else {
            bytes.push(byte);
            index += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
