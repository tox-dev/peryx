//! The legacy `PyPI` JSON API shape, serialized from a resolved Simple API detail page.

use std::collections::{BTreeMap as OrderedMap, BTreeSet};
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::version::{VersionKey, version_key};
use crate::{
    CoreMetadataDoc, File, ProjectDetail, Yanked, distribution_version_segment, file_matches_version,
    parse_distribution_filename, parse_version, sorted_desc,
};

/// Bucket a project's files by release version in a single pass, so rendering every release is linear
/// in the number of files rather than files times versions.
fn group_by_version(detail: &ProjectDetail) -> OrderedMap<VersionKey, Vec<&File>> {
    let mut groups: OrderedMap<VersionKey, Vec<&File>> = OrderedMap::new();
    for file in &detail.files {
        if let Some(candidate) = distribution_version_segment(&file.filename) {
            groups.entry(version_key(candidate)).or_default().push(file);
        }
    }
    groups
}

/// Serialize `GET /pypi/{project}/json` or `GET /pypi/{project}/{version}/json`.
///
/// Returns `None` when the requested version is not present in the resolved detail page.
///
/// `metadata` supplies the release's core-metadata contact fields, which the Simple API omits; `None`
/// leaves them empty.
///
/// # Panics
/// Never in practice: the model contains only string-keyed maps and plain values, which
/// `serde_json` always serializes.
#[must_use]
pub fn render_legacy_json(
    detail: &ProjectDetail,
    version: Option<&str>,
    metadata: Option<&CoreMetadataDoc>,
) -> Option<String> {
    let selected_version = match version {
        Some(version) => Some(find_release_version(detail, version)?),
        None => latest_release_version(detail),
    };
    let mut response = Map::new();
    response.insert(
        "info".to_owned(),
        legacy_info(detail, selected_version.as_deref(), metadata),
    );
    response.insert("last_serial".to_owned(), json!(0));
    if version.is_none() {
        response.insert("releases".to_owned(), legacy_releases(detail));
    }
    response.insert("urls".to_owned(), legacy_files(detail, selected_version.as_deref()));
    response.insert("vulnerabilities".to_owned(), json!([]));
    response.insert("ownership".to_owned(), json!({"roles": [], "organization": null}));
    Some(serde_json::to_string(&Value::Object(response)).expect("legacy JSON API model always serializes"))
}

fn legacy_info(detail: &ProjectDetail, version: Option<&str>, metadata: Option<&CoreMetadataDoc>) -> Value {
    let first_file = version.and_then(|version| release_files(detail, version).next());
    let requires_python = first_file.and_then(|file| file.requires_python.as_deref());
    let yanked = version.is_some_and(|version| release_yanked(detail, version));
    let yanked_reason = version.and_then(|version| release_yanked_reason(detail, version));
    let contact = |field: fn(&CoreMetadataDoc) -> Option<&str>| metadata.and_then(field).unwrap_or_default();
    json!({
        "author": contact(|doc| doc.author.as_deref()),
        "author_email": contact(|doc| doc.author_email.as_deref()),
        "bugtrack_url": null,
        "classifiers": [],
        "description": "",
        "description_content_type": null,
        "docs_url": null,
        "download_url": "",
        "downloads": {"last_day": -1, "last_month": -1, "last_week": -1},
        "dynamic": [],
        "home_page": "",
        "keywords": "",
        "license": "",
        "license_expression": null,
        "license_files": null,
        "maintainer": contact(|doc| doc.maintainer.as_deref()),
        "maintainer_email": contact(|doc| doc.maintainer_email.as_deref()),
        "name": &detail.name,
        "package_url": "",
        "platform": null,
        "project_url": "",
        "project_urls": {},
        "provides_extra": [],
        "release_url": "",
        "requires_dist": [],
        "requires_python": requires_python,
        "summary": "",
        "version": version.unwrap_or_default(),
        "yanked": yanked,
        "yanked_reason": yanked_reason,
    })
}

fn legacy_releases(detail: &ProjectDetail) -> Value {
    let groups = group_by_version(detail);
    let mut releases = Map::new();
    for version in release_versions(detail) {
        let files = groups
            .get(&version_key(&version))
            .map_or_else(Vec::new, |files| files.iter().map(|file| legacy_file(file)).collect());
        releases.insert(version, Value::Array(files));
    }
    Value::Object(releases)
}

fn legacy_files(detail: &ProjectDetail, version: Option<&str>) -> Value {
    let files = version.map_or_else(Vec::new, |version| {
        release_files(detail, version).map(legacy_file).collect()
    });
    Value::Array(files)
}

fn legacy_file(file: &File) -> Value {
    json!({
        "comment_text": "",
        "digests": &file.hashes,
        "downloads": -1,
        "filename": &file.filename,
        "has_sig": file.gpg_sig.unwrap_or(false),
        "md5_digest": file.hashes.get("md5").map(String::as_str),
        "packagetype": packagetype(&file.filename),
        "python_version": python_version(&file.filename),
        "requires_python": &file.requires_python,
        "size": file.size,
        "upload_time": file.upload_time.as_deref().map(legacy_upload_time),
        "upload_time_iso_8601": &file.upload_time,
        "url": &file.url,
        "yanked": yanked_bool(&file.yanked),
        "yanked_reason": yanked_reason(&file.yanked),
    })
}

fn find_release_version(detail: &ProjectDetail, requested: &str) -> Option<String> {
    for version in &detail.versions {
        if versions_match(version, requested) {
            return Some(version.clone());
        }
    }
    if release_files(detail, requested).next().is_some() {
        Some(requested.to_owned())
    } else {
        None
    }
}

fn latest_release_version(detail: &ProjectDetail) -> Option<String> {
    let versions = release_versions(detail);
    for version in &versions {
        if release_files(detail, version).next().is_some() {
            return Some(version.clone());
        }
    }
    versions.into_iter().next()
}

fn release_versions(detail: &ProjectDetail) -> Vec<String> {
    let mut versions: Vec<String> = detail.versions.clone();
    let mut seen: BTreeSet<VersionKey> = detail.versions.iter().map(|version| version_key(version)).collect();
    for file in &detail.files {
        let Some(version) = filename_version(&file.filename) else {
            continue;
        };
        if seen.insert(version_key(&version)) {
            versions.push(version);
        }
    }
    sorted_desc(&versions)
}

fn release_files<'a>(detail: &'a ProjectDetail, version: &'a str) -> impl Iterator<Item = &'a File> + 'a {
    detail
        .files
        .iter()
        .filter(move |file| file_matches_version(&file.filename, version))
}

fn release_yanked(detail: &ProjectDetail, version: &str) -> bool {
    let mut found = false;
    for file in release_files(detail, version) {
        found = true;
        if !yanked_bool(&file.yanked) {
            return false;
        }
    }
    found
}

fn release_yanked_reason<'a>(detail: &'a ProjectDetail, version: &'a str) -> Option<&'a str> {
    for file in release_files(detail, version) {
        if let Some(reason) = yanked_reason(&file.yanked) {
            return Some(reason);
        }
    }
    None
}

const fn yanked_bool(yanked: &Yanked) -> bool {
    !matches!(yanked, Yanked::No)
}

fn yanked_reason(yanked: &Yanked) -> Option<&str> {
    match yanked {
        Yanked::Reason(reason) => Some(reason),
        Yanked::No | Yanked::Yes => None,
    }
}

fn filename_version(filename: &str) -> Option<String> {
    let parsed = parse_distribution_filename(filename).ok()?;
    Some(parsed.version.to_string())
}

fn versions_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let (Some(left), Some(right)) = (parse_version(left), parse_version(right)) else {
        return false;
    };
    left == right
}

fn packagetype(filename: &str) -> &'static str {
    if extension_eq(filename, "whl") {
        "bdist_wheel"
    } else if extension_eq(filename, "egg") {
        "bdist_egg"
    } else {
        "sdist"
    }
}

fn python_version(filename: &str) -> &str {
    if !extension_eq(filename, "whl") {
        return "source";
    }
    // A wheel name is name-version[-build]-python-abi-platform, so the tag is always third from the
    // end. Collecting the stem allocated a Vec per file just to read one of its middle elements.
    let stem = &filename[..filename.len() - 4];
    match stem.split('-').count() {
        5 | 6 => stem.rsplit('-').nth(2).unwrap_or("source"),
        _ => "source",
    }
}

fn extension_eq(filename: &str, extension: &str) -> bool {
    Path::new(filename)
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn legacy_upload_time(upload_time: &str) -> &str {
    let without_z = upload_time.strip_suffix('Z').unwrap_or(upload_time);
    match without_z.split_once('.') {
        Some((whole, _)) => whole,
        None => without_z,
    }
}
