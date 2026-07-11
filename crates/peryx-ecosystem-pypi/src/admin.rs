//! The `PyPI` half of peryx's cache-maintenance commands: which stored blobs its metadata tables
//! reference, and whether those tables are internally consistent. The neutral binary drives the
//! blob store itself (content-addressed, so ecosystem-agnostic) and dispatches the metadata half
//! here through the ecosystem driver.

use std::collections::BTreeSet;
use std::io::Write;

use peryx_driver::serving::{CachePage, PurgeReport};
use peryx_index::Index;
use peryx_policy::{PolicyAction, PolicyDenial};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;

use crate::store::CachedIndex;
use crate::store::PypiStore as _;

use crate::policy::PypiPolicy as _;
use crate::upload::Uploaded;
use crate::{CoreMetadata, ProjectDetail, normalize_name, parse_detail};

/// The blob digests every `PyPI` metadata table references: cached file URLs, PEP 658 metadata
/// siblings, and hosted upload records. The neutral orphan-blob collector keeps these and reclaims
/// the rest.
///
/// # Errors
/// Returns a message when a metadata record is malformed, since a purge must not run against a store
/// it cannot fully account for.
pub fn referenced_blob_digests(meta: &MetaStore) -> Result<BTreeSet<String>, String> {
    let mut digests = BTreeSet::new();
    meta.scan_file_urls(|digest, value| {
        if Digest::from_hex(digest).is_none() || split_pair(value).is_none() {
            return Err(format!("invalid file URL record for digest {digest:?}"));
        }
        digests.insert(digest.to_owned());
        Ok(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_metadata_records(|digest, value| {
        let Some((_url, metadata_digest, _source)) = split_triple(value) else {
            return Err(format!("invalid PEP 658 metadata record for digest {digest:?}"));
        };
        if Digest::from_hex(digest).is_none() {
            return Err(format!("invalid PEP 658 wheel digest {digest:?}"));
        }
        if Digest::from_hex(metadata_digest).is_none() {
            return Err(format!("invalid PEP 658 metadata digest {metadata_digest:?}"));
        }
        digests.insert(digest.to_owned());
        digests.insert(metadata_digest.to_owned());
        Ok(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_upload_records(|key, bytes| {
        for digest in upload_digests(bytes).ok_or_else(|| format!("invalid upload record {key}"))? {
            digests.insert(digest.as_str().to_owned());
        }
        Ok::<(), String>(())
    })
    .map_err(|err| err.to_string())?;
    Ok(digests)
}

/// This driver's cached index pages, each key split into `(index, project)`, for `cache list`/`cache
/// size`. `index_names` are the configured index names, longest first, so a slash-bearing key splits
/// against a real index rather than at its first slash.
///
/// # Errors
/// Returns a message when the store cannot be read.
pub fn cache_pages(meta: &MetaStore, index_names: &[&str]) -> Result<Vec<CachePage>, String> {
    let mut pages = Vec::new();
    meta.scan_index_pages(|page| {
        let (index, project) = split_page_key(&page.key, index_names);
        pages.push(CachePage {
            index,
            project,
            fetched_at_unix: page.summary.fetched_at_unix,
            fresh_secs: page.summary.fresh_secs,
            body_bytes: page.summary.body_bytes,
            record_bytes: page.summary.record_bytes,
            key: page.key,
        });
        Ok::<(), std::convert::Infallible>(())
    })
    .map_err(|err| err.to_string())?;
    Ok(pages)
}

/// This driver's cached metadata record counts, labeled by kind, for `cache size`.
///
/// # Errors
/// Returns a message when the store cannot be read.
pub fn cache_record_counts(meta: &MetaStore) -> Result<Vec<(String, u64)>, String> {
    let mut file_urls = 0_u64;
    let mut metadata = 0_u64;
    let mut projects = 0_u64;
    let mut uploads = 0_u64;
    let mut overrides = 0_u64;
    meta.scan_file_urls(|_digest, _value| {
        file_urls += 1;
        Ok::<(), std::convert::Infallible>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_metadata_records(|_digest, _value| {
        metadata += 1;
        Ok::<(), std::convert::Infallible>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_project_records(|_key, _display| {
        projects += 1;
        Ok::<(), std::convert::Infallible>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_upload_records(|_key, _bytes| {
        uploads += 1;
        Ok::<(), std::convert::Infallible>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_override_records(|_key, _kind| {
        overrides += 1;
        Ok::<(), std::convert::Infallible>(())
    })
    .map_err(|err| err.to_string())?;
    Ok(vec![
        ("file_url_records".to_owned(), file_urls),
        ("metadata_records".to_owned(), metadata),
        ("project_records".to_owned(), projects),
        ("upload_records".to_owned(), uploads),
        ("override_records".to_owned(), overrides),
    ])
}

/// Preview this ecosystem's policy decisions over its cached and uploaded records, writing one
/// tab-separated line per denial to `out`. `index_filter` restricts to one index by name or route;
/// `project_filter` restricts to one normalized project. `indexes` is every configured index, of
/// which this considers only the `PyPI` ones its records belong to.
///
/// # Errors
/// Returns a message when a record cannot be read or `out` cannot be written.
pub fn policy_dry_run(
    meta: &MetaStore,
    indexes: &[Index],
    index_filter: Option<&str>,
    project_filter: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), String> {
    let mut names = indexes.iter().map(|index| index.name.as_str()).collect::<Vec<_>>();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    let project_filter = project_filter.map(normalize_name);
    meta.scan_index_records(|key, bytes| {
        let (index_name, project) = split_page_key(key, &names);
        let Some(index) = matching_index(indexes, &index_name, index_filter) else {
            return Ok(());
        };
        if project_filter.as_deref().is_some_and(|filter| filter != project) {
            return Ok(());
        }
        let record = CachedIndex::decode(bytes).map_err(|err| format!("corrupt cached page {key}: {err}"))?;
        let parsed = parse_detail(&record.body).map_err(|err| err.to_string())?;
        let detail = ProjectDetail {
            meta: parsed.meta,
            name: project,
            versions: parsed.versions,
            files: parsed.files,
        };
        for denial in index.policy.preview_detail(PolicyAction::Serve, &detail) {
            write_denial(out, &index.name, &denial).map_err(|err| err.to_string())?;
        }
        Ok::<(), String>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_upload_records(|key, bytes| {
        let Some((index_name, project, _filename)) = upload_key_parts(key, &names) else {
            return Ok(());
        };
        let Some(index) = matching_index(indexes, &index_name, index_filter) else {
            return Ok(());
        };
        if project_filter.as_deref().is_some_and(|filter| filter != project) {
            return Ok(());
        }
        let uploaded: Uploaded = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
        if let Err(denial) = index.policy.check_file(PolicyAction::Upload, project, &uploaded.file) {
            write_denial(out, &index.name, &denial).map_err(|err| err.to_string())?;
        }
        Ok::<(), String>(())
    })
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn matching_index<'a>(indexes: &'a [Index], index_name: &str, filter: Option<&str>) -> Option<&'a Index> {
    let index = indexes.iter().find(|index| index.name == index_name)?;
    filter
        .is_none_or(|filter| filter == index.name || filter == index.route)
        .then_some(index)
}

fn write_denial(out: &mut dyn Write, index: &str, denial: &PolicyDenial) -> std::io::Result<()> {
    writeln!(
        out,
        "{}\t{index}\t{}\t{}\t{}\t{}\t{}\t{}",
        denial.action,
        denial.project,
        denial.filename.as_deref().unwrap_or(""),
        denial.version.as_deref().unwrap_or(""),
        denial.rule,
        denial.field,
        denial.reason
    )
}

fn split_page_key(key: &str, index_names: &[&str]) -> (String, String) {
    for name in index_names {
        if let Some(project) = key.strip_prefix(name).and_then(|rest| rest.strip_prefix('/')) {
            return ((*name).to_owned(), project.to_owned());
        }
    }
    key.split_once('/').map_or_else(
        || (key.to_owned(), String::new()),
        |(index, project)| (index.to_owned(), project.to_owned()),
    )
}

fn upload_key_parts<'a>(key: &'a str, index_names: &[&str]) -> Option<(String, &'a str, &'a str)> {
    for name in index_names {
        let Some(rest) = key.strip_prefix(name).and_then(|rest| rest.strip_prefix('/')) else {
            continue;
        };
        let (project, filename) = rest.split_once('/')?;
        return Some(((*name).to_owned(), project, filename));
    }
    let (index, rest) = key.split_once('/')?;
    let (project, filename) = rest.split_once('/')?;
    Some((index.to_owned(), project, filename))
}

/// Purge one project's cached records from `index`, keeping any blob a still-cached project or a
/// hosted upload also references. With `apply`, deletes the records and returns the removed counts;
/// otherwise counts what a purge would remove. Returns the normalized project name alongside.
///
/// # Errors
/// Returns a message when a cached page cannot be read or the store cannot be written.
pub fn purge_project(meta: &MetaStore, index: &str, project: &str, apply: bool) -> Result<PurgeReport, String> {
    let normalized = normalize_name(project);
    let target_key = format!("{index}/{normalized}");
    let target = project_refs(meta, &target_key)?;
    let preserved = preserved_refs(meta, &target_key)?;
    let file_digests = target.files.difference(&preserved.files).cloned().collect::<Vec<_>>();
    let metadata_digests = target
        .metadata_wheels
        .difference(&preserved.files)
        .cloned()
        .collect::<Vec<_>>();
    let counts = if apply {
        meta.delete_project_cache(index, &normalized, &file_digests, &metadata_digests)
            .map_err(|err| err.to_string())?
    } else {
        meta.count_project_cache_purge(index, &normalized, &file_digests, &metadata_digests)
            .map_err(|err| err.to_string())?
    };
    Ok(PurgeReport {
        project: normalized,
        categories: vec![
            ("index_pages".to_owned(), counts.index_pages as u64),
            ("project_records".to_owned(), counts.project_records as u64),
            ("file_url_records".to_owned(), counts.file_url_records as u64),
            ("metadata_records".to_owned(), counts.metadata_records as u64),
        ],
    })
}

#[derive(Default)]
struct CacheRefs {
    files: BTreeSet<String>,
    metadata_wheels: BTreeSet<String>,
}

fn project_refs(meta: &MetaStore, target_key: &str) -> Result<CacheRefs, String> {
    let Some(record) = meta
        .get_index(target_key)
        .map_err(|err| format!("read cached project {target_key}: {err}"))?
    else {
        return Ok(CacheRefs::default());
    };
    let mut refs = CacheRefs::default();
    add_index_refs(&mut refs, &record).map_err(|err| format!("read cached project {target_key}: {err}"))?;
    Ok(refs)
}

fn preserved_refs(meta: &MetaStore, target_key: &str) -> Result<CacheRefs, String> {
    let mut refs = CacheRefs::default();
    meta.scan_index_records(|key, bytes| {
        if key == target_key {
            return Ok(());
        }
        let record = CachedIndex::decode(bytes).map_err(|err| format!("corrupt cached page {key}: {err}"))?;
        add_index_refs(&mut refs, &record).map_err(|err| format!("corrupt cached page {key}: {err}"))
    })
    .map_err(|err| err.to_string())?;
    meta.scan_upload_records(|key, bytes| {
        for digest in upload_digests(bytes).ok_or_else(|| format!("invalid upload record {key}"))? {
            refs.files.insert(digest.as_str().to_owned());
        }
        Ok::<(), String>(())
    })
    .map_err(|err| err.to_string())?;
    Ok(refs)
}

fn add_index_refs(refs: &mut CacheRefs, record: &CachedIndex) -> Result<(), String> {
    for file in parse_detail(&record.body).map_err(|err| err.to_string())?.files {
        let Some(sha256) = file.hashes.get("sha256") else {
            continue;
        };
        if Digest::from_hex(sha256).is_none() {
            return Err(format!("cached page contains invalid sha256 digest {sha256:?}"));
        }
        refs.files.insert(sha256.to_owned());
        if let CoreMetadata::Hashes(hashes) = file.core_metadata
            && let Some(metadata_digest) = hashes.get("sha256")
        {
            if Digest::from_hex(metadata_digest).is_none() {
                return Err(format!(
                    "cached page contains invalid metadata digest {metadata_digest:?}"
                ));
            }
            refs.metadata_wheels.insert(sha256.to_owned());
        }
    }
    Ok(())
}

/// Validate every `PyPI` metadata record in `meta`, writing one tab-separated line per problem to
/// `out` and returning the count. Blob contents are the neutral caller's to verify.
///
/// # Errors
/// Returns a message when the store cannot be read or `out` cannot be written.
pub fn fsck_metadata(meta: &MetaStore, blobs: &BlobStore, out: &mut dyn Write) -> Result<u64, String> {
    let mut problems = 0_u64;
    meta.scan_index_records(|key, bytes| {
        match CachedIndex::decode(bytes) {
            Ok(record) if parse_detail(&record.body).is_ok() => {}
            Ok(_) => {
                problems += 1;
                writeln!(out, "metadata\tindex\t{key}\tinvalid project detail")?;
            }
            Err(err) => {
                problems += 1;
                writeln!(out, "metadata\tindex\t{key}\t{err}")?;
            }
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_file_urls(|digest, value| {
        if Digest::from_hex(digest).is_none() || split_pair(value).is_none() {
            problems += 1;
            writeln!(out, "metadata\tfile-url\t{digest}\tinvalid record")?;
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_metadata_records(|digest, value| {
        let valid = Digest::from_hex(digest).is_some()
            && split_triple(value)
                .is_some_and(|(_url, metadata_digest, _source)| Digest::from_hex(metadata_digest).is_some());
        if !valid {
            problems += 1;
            writeln!(out, "metadata\tpep658\t{digest}\tinvalid record")?;
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_project_records(|key, display| {
        if !valid_project_key(key) || display.is_empty() {
            problems += 1;
            writeln!(out, "metadata\tproject\t{key}\tinvalid record")?;
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_upload_records(|key, bytes| {
        let Some(digests) = upload_digests(bytes) else {
            problems += 1;
            writeln!(out, "metadata\tupload\t{key}\tinvalid record")?;
            return Ok(());
        };
        if !valid_upload_key(key) {
            problems += 1;
            writeln!(out, "metadata\tupload\t{key}\tinvalid key")?;
            return Ok(());
        }
        for digest in digests {
            if !blobs.exists(&digest) {
                problems += 1;
                writeln!(out, "metadata\tupload\t{key}\tmissing blob {}", digest.as_str())?;
            }
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    meta.scan_override_records(|key, kind| {
        if !valid_upload_key(key) || !matches!(kind, "hidden" | "yanked") {
            problems += 1;
            writeln!(out, "metadata\toverride\t{key}\tinvalid record")?;
        }
        Ok::<(), std::io::Error>(())
    })
    .map_err(|err| err.to_string())?;
    Ok(problems)
}

/// The stored-blob digests one upload record names: its distribution file, and the PEP 658 metadata
/// sibling when the upload carried one. `None` when the record does not deserialize.
fn upload_digests(bytes: &[u8]) -> Option<Vec<Digest>> {
    let upload: Uploaded = serde_json::from_slice(bytes).ok()?;
    let mut digests = vec![Digest::from_hex(upload.file.hashes.get("sha256")?)?];
    if let CoreMetadata::Hashes(hashes) = upload.file.core_metadata
        && let Some(metadata_digest) = hashes.get("sha256")
    {
        digests.push(Digest::from_hex(metadata_digest)?);
    }
    Some(digests)
}

fn split_pair(value: &str) -> Option<(&str, &str)> {
    value.split_once('\n')
}

fn split_triple(value: &str) -> Option<(&str, &str, &str)> {
    let mut parts = value.splitn(3, '\n');
    Some((parts.next()?, parts.next()?, parts.next()?))
}

fn valid_project_key(key: &str) -> bool {
    key.split_once('/')
        .is_some_and(|(index, project)| !index.is_empty() && !project.is_empty())
}

fn valid_upload_key(key: &str) -> bool {
    let mut parts = key.splitn(3, '/');
    parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
}
