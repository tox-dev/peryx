//! The legacy `PyPI` JSON API shape, serialized from a resolved Simple API detail page.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::{
    File, ProjectDetail, Yanked, file_matches_version, parse_distribution_filename, parse_version, sorted_desc,
};

/// Serialize `GET /pypi/{project}/json` or `GET /pypi/{project}/{version}/json`.
///
/// Returns `None` when the requested version is not present in the resolved detail page.
///
/// # Panics
/// Never in practice: the model contains only string-keyed maps and plain values, which
/// `serde_json` always serializes.
#[must_use]
pub fn render_legacy_json(detail: &ProjectDetail, version: Option<&str>) -> Option<String> {
    let selected_version = match version {
        Some(version) => Some(find_release_version(detail, version)?),
        None => latest_release_version(detail),
    };
    let mut response = Map::new();
    response.insert("info".to_owned(), legacy_info(detail, selected_version.as_deref()));
    response.insert("last_serial".to_owned(), json!(0));
    if version.is_none() {
        response.insert("releases".to_owned(), legacy_releases(detail));
    }
    response.insert("urls".to_owned(), legacy_files(detail, selected_version.as_deref()));
    response.insert("vulnerabilities".to_owned(), json!([]));
    response.insert("ownership".to_owned(), json!({"roles": [], "organization": null}));
    Some(serde_json::to_string(&Value::Object(response)).expect("legacy JSON API model always serializes"))
}

fn legacy_info(detail: &ProjectDetail, version: Option<&str>) -> Value {
    let first_file = version.and_then(|version| release_files(detail, version).next());
    let requires_python = first_file.and_then(|file| file.requires_python.as_deref());
    let yanked = version.is_some_and(|version| release_yanked(detail, version));
    let yanked_reason = version.and_then(|version| release_yanked_reason(detail, version));
    json!({
        "author": "",
        "author_email": "",
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
        "maintainer": "",
        "maintainer_email": "",
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
    let mut releases = Map::new();
    for version in release_versions(detail) {
        releases.insert(version.clone(), legacy_files(detail, Some(&version)));
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
    let mut versions = sorted_desc(&detail.versions);
    for file in &detail.files {
        let Some(version) = filename_version(&file.filename) else {
            continue;
        };
        let mut seen = false;
        for known in &versions {
            if versions_match(known, &version) {
                seen = true;
                break;
            }
        }
        if !seen {
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
    if let Ok(parsed) = parse_distribution_filename(filename) {
        return Some(parsed.version.to_string());
    }
    if !extension_eq(filename, "zip") {
        return None;
    }
    let stem = &filename[..filename.len() - 4];
    let (_, version) = stem.rsplit_once('-')?;
    Some(version.to_owned())
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
    let stem = &filename[..filename.len() - 4];
    let parts: Vec<&str> = stem.split('-').collect();
    match parts.as_slice() {
        [_name, _version, python, _abi, _platform] => python,
        [_name, _version, _build, python, _abi, _platform] => python,
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
