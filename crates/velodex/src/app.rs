//! Command actions that do not touch global state.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use velodex_ecosystem_pypi::{CoreMetadata, ProjectDetail, normalize_name, parse_detail};
use velodex_http::discovery::{BaseUrl, SnippetKind, snippet_text};
use velodex_http::upload::Uploaded;
use velodex_policy::{PolicyAction, PolicyDenial};
use velodex_storage::blob::{BlobEntry, BlobStore, Digest};
use velodex_storage::meta::{CachedIndex, MetaStore, ProjectCachePurgeCounts};

use crate::cli::{
    CacheCommand, CacheListArgs, CachePurgeCommand, CachePurgeOrphanedBlobsArgs, CachePurgeProjectArgs, PolicyCommand,
    PolicyDryRunArgs,
};
use crate::config::Config;
use crate::server;

/// Create the data directory if it is missing. Returns whether it was created.
///
/// # Errors
/// Propagates the filesystem error when the directory cannot be created.
pub fn init_data_dir(data_dir: &Path) -> std::io::Result<bool> {
    if data_dir.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(data_dir)?;
    Ok(true)
}

/// Run `velodex init`: ensure the data directory exists.
///
/// # Errors
/// Propagates the filesystem error when the directory cannot be created.
pub fn init(config: &Config) -> anyhow::Result<()> {
    if init_data_dir(&config.data_dir)? {
        tracing::info!(path = %config.data_dir.display(), "initialized data directory");
    } else {
        tracing::info!(path = %config.data_dir.display(), "data directory already exists");
    }
    Ok(())
}

/// Render one client configuration snippet from the configured index topology.
///
/// # Errors
/// Returns an error if the base URL is invalid, the index route is unknown, or the requested
/// snippet needs uploads on a read-only index.
pub fn config_snippet(config: &Config, route: &str, base_url: &str, kind: SnippetKind) -> anyhow::Result<String> {
    let base = BaseUrl::parse(base_url)?;
    let index = velodex_http::describe_indexes(&server::build_indexes(&config.indexes, config.offline)?)
        .into_iter()
        .find(|index| index.route == route)
        .with_context(|| format!("unknown index route {route:?}"))?;
    let Some(text) = snippet_text(&base, &index.route, index.uploads, kind) else {
        bail!("index route {route:?} does not accept uploads");
    };
    Ok(text)
}

/// Run a cache inspection or maintenance command.
///
/// # Errors
/// Returns an error if the metadata store or blob store cannot be read, or if output fails.
pub fn cache(config: &Config, command: &CacheCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    cache_at(config, command, unix_now(), out)
}

/// Run a policy inspection command.
///
/// # Errors
/// Returns an error if configured indexes cannot be built, the metadata store cannot be read, or
/// output fails.
pub fn policy(config: &Config, command: &PolicyCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    let stores = CacheStores::open(config)?;
    let indexes = server::build_indexes(&config.indexes, config.offline)?;
    match command {
        PolicyCommand::DryRun(args) => policy_dry_run(config, &stores, &indexes, args, out),
    }
}

fn policy_dry_run(
    config: &Config,
    stores: &CacheStores,
    indexes: &[velodex_http::Index],
    args: &PolicyDryRunArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    writeln!(out, "action\tindex\tproject\tfilename\tversion\trule\tfield\treason")?;
    let names = index_names(config);
    let project_filter = args.project.as_deref().map(normalize_name);
    stores
        .meta
        .scan_index_records(|key, bytes| {
            let (index_name, project) = split_page_key(key, &names);
            let Some(index) = matching_policy_index(indexes, &index_name, args.index.as_deref()) else {
                return Ok::<(), std::io::Error>(());
            };
            if project_filter
                .as_deref()
                .is_some_and(|filter| filter != project.as_str())
            {
                return Ok::<(), std::io::Error>(());
            }
            let record = CachedIndex::decode(bytes).map_err(std::io::Error::other)?;
            let parsed = parse_detail(&record.body).map_err(std::io::Error::other)?;
            let detail = ProjectDetail {
                meta: parsed.meta,
                name: project,
                versions: parsed.versions,
                files: parsed.files,
            };
            for denial in index.policy.preview_detail(PolicyAction::Serve, &detail) {
                write_policy_denial(out, &index.name, &denial)?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan cached index records")?;
    stores
        .meta
        .scan_upload_records(|key, bytes| {
            let Some((index_name, project, _filename)) = upload_key_parts(key, &names) else {
                return Ok::<(), std::io::Error>(());
            };
            let Some(index) = matching_policy_index(indexes, &index_name, args.index.as_deref()) else {
                return Ok::<(), std::io::Error>(());
            };
            if project_filter.as_deref().is_some_and(|filter| filter != project) {
                return Ok::<(), std::io::Error>(());
            }
            let uploaded: Uploaded = serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
            if let Err(denial) = index.policy.check_file(PolicyAction::Upload, project, &uploaded.file) {
                write_policy_denial(out, &index.name, &denial)?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan upload records")?;
    Ok(())
}

fn matching_policy_index<'a>(
    indexes: &'a [velodex_http::Index],
    index_name: &str,
    filter: Option<&str>,
) -> Option<&'a velodex_http::Index> {
    let index = indexes.iter().find(|index| index.name == index_name)?;
    filter
        .is_none_or(|filter| filter == index.name || filter == index.route)
        .then_some(index)
}

fn write_policy_denial(out: &mut dyn Write, index: &str, denial: &PolicyDenial) -> std::io::Result<()> {
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

fn cache_at(config: &Config, command: &CacheCommand, now: i64, out: &mut dyn Write) -> anyhow::Result<()> {
    let stores = CacheStores::open(config)?;
    match command {
        CacheCommand::List(args) => list_cache(config, &stores, args, now, out),
        CacheCommand::Size(_) => size_cache(config, &stores, now, out),
        CacheCommand::Fsck(_) => fsck_cache(&stores, out),
        CacheCommand::Purge(CachePurgeCommand::Project(args)) => purge_project(&stores, args, out),
        CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(args)) => purge_orphaned_blobs(&stores, args, out),
    }
}

struct CacheStores {
    meta: MetaStore,
    blobs: BlobStore,
}

impl CacheStores {
    fn open(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            meta: MetaStore::open_existing(config.data_dir.join("velodex.redb"))
                .with_context(|| format!("open metadata store {}", config.data_dir.join("velodex.redb").display()))?,
            blobs: BlobStore::new(config.data_dir.join("blobs")),
        })
    }
}

fn list_cache(
    config: &Config,
    stores: &CacheStores,
    args: &CacheListArgs,
    now: i64,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let project = args.project.as_deref().map(normalize_name);
    let index_names = index_names(config);
    writeln!(
        out,
        "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey"
    )?;
    if args.digest.is_none() {
        stores
            .meta
            .scan_index_pages(|page| {
                let (index, project_name) = split_page_key(&page.key, &index_names);
                let age = age_secs(now, page.summary.fetched_at_unix);
                let ttl = page.summary.fresh_secs.unwrap_or(config.cache_ttl_secs);
                let stale = is_stale(age, ttl);
                if args.index.as_deref().is_some_and(|filter| filter != index.as_str())
                    || project.as_deref().is_some_and(|filter| filter != project_name.as_str())
                    || args.stale && !stale
                    || args.min_age_secs.is_some_and(|min| age < min)
                    || args.min_size_bytes.is_some_and(|min| page.summary.body_bytes < min)
                {
                    return Ok(());
                }
                writeln!(
                    out,
                    "index\t{index}\t{project_name}\t\t{age}\t{}\t{stale}\t{}\t{}",
                    page.summary
                        .fresh_secs
                        .map_or_else(|| "-".to_owned(), |secs| secs.to_string()),
                    page.summary.body_bytes,
                    page.key,
                )
            })
            .context("scan cached index pages")?;
    }
    if args.index.is_some() || args.project.is_some() || args.stale || args.min_age_secs.is_some() {
        return Ok(());
    }
    stores
        .blobs
        .scan(|entry| {
            let Some(digest) = &entry.digest else {
                return Ok(());
            };
            if args.digest.as_deref().is_some_and(|filter| filter != digest.as_str())
                || args.min_size_bytes.is_some_and(|min| entry.bytes < min)
            {
                return Ok(());
            }
            writeln!(
                out,
                "blob\t\t\t{}\t-\t-\t-\t{}\t{}",
                digest.as_str(),
                entry.bytes,
                entry.path.display()
            )
        })
        .context("scan blob files")?;
    Ok(())
}

fn size_cache(config: &Config, stores: &CacheStores, now: i64, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut index_pages = 0_u64;
    let mut index_bytes = 0_u64;
    let mut stale_index_pages = 0_u64;
    stores
        .meta
        .scan_index_pages(|page| {
            index_pages += 1;
            index_bytes += page.summary.record_bytes;
            let age = age_secs(now, page.summary.fetched_at_unix);
            let ttl = page.summary.fresh_secs.unwrap_or(config.cache_ttl_secs);
            stale_index_pages += u64::from(is_stale(age, ttl));
            Ok::<(), std::io::Error>(())
        })
        .context("scan cached index pages")?;

    let mut blob_files = 0_u64;
    let mut blob_bytes = 0_u64;
    let mut invalid_blob_paths = 0_u64;
    stores
        .blobs
        .scan(|entry| {
            blob_files += 1;
            blob_bytes += entry.bytes;
            invalid_blob_paths += u64::from(entry.digest.is_none());
            Ok::<(), std::io::Error>(())
        })
        .context("scan blob files")?;

    let counts = metadata_counts(&stores.meta)?;
    writeln!(out, "index_pages\t{index_pages}")?;
    writeln!(out, "stale_index_pages\t{stale_index_pages}")?;
    writeln!(out, "index_bytes\t{index_bytes}")?;
    writeln!(out, "blob_files\t{blob_files}")?;
    writeln!(out, "blob_bytes\t{blob_bytes}")?;
    writeln!(out, "invalid_blob_paths\t{invalid_blob_paths}")?;
    writeln!(out, "file_url_records\t{}", counts.file_urls)?;
    writeln!(out, "metadata_records\t{}", counts.metadata)?;
    writeln!(out, "project_records\t{}", counts.projects)?;
    writeln!(out, "upload_records\t{}", counts.uploads)?;
    writeln!(out, "override_records\t{}", counts.overrides)?;
    Ok(())
}

fn fsck_cache(stores: &CacheStores, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut problems = 0_u64;
    stores
        .meta
        .scan_index_records(|key, bytes| {
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
        .context("scan index metadata")?;
    stores
        .meta
        .scan_file_urls(|digest, value| {
            if Digest::from_hex(digest).is_none() || split_pair(value).is_none() {
                problems += 1;
                writeln!(out, "metadata\tfile-url\t{digest}\tinvalid record")?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan file URL metadata")?;
    stores
        .meta
        .scan_metadata_records(|digest, value| {
            let valid = Digest::from_hex(digest).is_some()
                && split_triple(value)
                    .is_some_and(|(_url, metadata_digest, _source)| Digest::from_hex(metadata_digest).is_some());
            if !valid {
                problems += 1;
                writeln!(out, "metadata\tpep658\t{digest}\tinvalid record")?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan PEP 658 metadata")?;
    stores
        .meta
        .scan_project_records(|key, display| {
            if !valid_project_key(key) || display.is_empty() {
                problems += 1;
                writeln!(out, "metadata\tproject\t{key}\tinvalid record")?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan project metadata")?;
    stores
        .meta
        .scan_upload_records(|key, bytes| {
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
                if !stores.blobs.exists(&digest) {
                    problems += 1;
                    writeln!(out, "metadata\tupload\t{key}\tmissing blob {}", digest.as_str())?;
                }
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan upload metadata")?;
    stores
        .meta
        .scan_override_records(|key, kind| {
            if !valid_upload_key(key) || !matches!(kind, "hidden" | "yanked") {
                problems += 1;
                writeln!(out, "metadata\toverride\t{key}\tinvalid record")?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan override metadata")?;
    stores
        .blobs
        .scan(|entry| {
            problems += check_blob(stores, &entry, out)?;
            Ok::<(), std::io::Error>(())
        })
        .context("scan blob files")?;
    if problems == 0 {
        writeln!(out, "ok")?;
    } else {
        writeln!(out, "problems\t{problems}")?;
    }
    Ok(())
}

fn purge_project(stores: &CacheStores, args: &CachePurgeProjectArgs, out: &mut dyn Write) -> anyhow::Result<()> {
    let normalized = normalize_name(&args.project);
    let target_key = format!("{}/{}", args.index, normalized);
    let target_refs = project_refs(stores, &target_key)?;
    let preserved_refs = preserved_refs(stores, &target_key)?;
    let file_digests = target_refs
        .files
        .difference(&preserved_refs.files)
        .cloned()
        .collect::<Vec<_>>();
    let metadata_digests = target_refs
        .metadata_wheels
        .difference(&preserved_refs.files)
        .cloned()
        .collect::<Vec<_>>();
    let counts = if args.yes {
        stores
            .meta
            .delete_project_cache(&args.index, &normalized, &file_digests, &metadata_digests)
            .context("delete project cache metadata")?
    } else {
        stores
            .meta
            .count_project_cache_purge(&args.index, &normalized, &file_digests, &metadata_digests)
            .context("count project cache metadata")?
    };
    let header = b"action\ttarget\tindex\tproject\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n";
    out.write_all(header)?;
    write_project_purge_counts(
        out,
        if args.yes { "removed" } else { "dry-run" },
        &args.index,
        &normalized,
        counts,
    )
}

fn purge_orphaned_blobs(
    stores: &CacheStores,
    args: &CachePurgeOrphanedBlobsArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let references = referenced_blob_digests(&stores.meta)?;
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    writeln!(out, "action\ttarget\tdigest\tsize_bytes\tpath")?;
    stores
        .blobs
        .scan(|entry| {
            let Some(digest) = &entry.digest else {
                return Ok::<(), anyhow::Error>(());
            };
            if references.contains(digest.as_str()) {
                return Ok(());
            }
            count += 1;
            bytes += entry.bytes;
            if args.yes {
                stores.blobs.remove(digest)?;
            }
            writeln!(
                out,
                "{}\torphaned-blob\t{}\t{}\t{}",
                if args.yes { "removed" } else { "dry-run" },
                digest.as_str(),
                entry.bytes,
                entry.path.display()
            )?;
            Ok(())
        })
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("scan orphaned blob files")?;
    writeln!(
        out,
        "summary\t{}\torphaned-blobs\t{count}\t{bytes}",
        if args.yes { "removed" } else { "dry-run" }
    )?;
    Ok(())
}

fn check_blob(stores: &CacheStores, entry: &BlobEntry, out: &mut dyn Write) -> std::io::Result<u64> {
    let Some(digest) = &entry.digest else {
        writeln!(
            out,
            "blob\tpath\t{}\tinvalid content-addressed path",
            entry.path.display()
        )?;
        return Ok(1);
    };
    match stores.blobs.verify(digest) {
        Ok(true) => Ok(0),
        Ok(false) => {
            writeln!(out, "blob\thash\t{}\tdigest mismatch", digest.as_str())?;
            Ok(1)
        }
        Err(err) => {
            writeln!(out, "blob\tread\t{}\t{err}", digest.as_str())?;
            Ok(1)
        }
    }
}

#[derive(Default)]
struct MetadataCounts {
    file_urls: u64,
    metadata: u64,
    projects: u64,
    uploads: u64,
    overrides: u64,
}

fn metadata_counts(meta: &MetaStore) -> anyhow::Result<MetadataCounts> {
    let mut counts = MetadataCounts::default();
    meta.scan_file_urls(|_digest, _value| {
        counts.file_urls += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan file URL metadata")?;
    meta.scan_metadata_records(|_digest, _value| {
        counts.metadata += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan PEP 658 metadata")?;
    meta.scan_project_records(|_key, _display| {
        counts.projects += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan project metadata")?;
    meta.scan_upload_records(|_key, _bytes| {
        counts.uploads += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan upload metadata")?;
    meta.scan_override_records(|_key, _kind| {
        counts.overrides += 1;
        Ok::<(), std::io::Error>(())
    })
    .context("scan override metadata")?;
    Ok(counts)
}

#[derive(Default)]
struct CacheRefs {
    files: BTreeSet<String>,
    metadata_wheels: BTreeSet<String>,
}

fn project_refs(stores: &CacheStores, target_key: &str) -> anyhow::Result<CacheRefs> {
    let Some(record) = stores
        .meta
        .get_index(target_key)
        .with_context(|| format!("read cached project {target_key}"))?
    else {
        return Ok(CacheRefs::default());
    };
    let mut refs = CacheRefs::default();
    add_index_refs(&mut refs, &record).with_context(|| format!("read cached project digests for {target_key}"))?;
    Ok(refs)
}

fn preserved_refs(stores: &CacheStores, target_key: &str) -> anyhow::Result<CacheRefs> {
    let mut refs = CacheRefs::default();
    stores
        .meta
        .scan_index_records(|key, bytes| {
            if key == target_key {
                return Ok::<(), anyhow::Error>(());
            }
            let record = CachedIndex::decode(bytes).map_err(anyhow::Error::from)?;
            add_index_refs(&mut refs, &record)?;
            Ok(())
        })
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("scan cached pages for shared digests")?;
    stores
        .meta
        .scan_upload_records(|key, bytes| {
            for digest in upload_digests(bytes).with_context(|| format!("read upload record {key}"))? {
                refs.files.insert(digest.as_str().to_owned());
            }
            Ok::<(), anyhow::Error>(())
        })
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("scan upload records for shared digests")?;
    Ok(refs)
}

pub(crate) fn referenced_blob_digests(meta: &MetaStore) -> anyhow::Result<BTreeSet<String>> {
    let mut digests = BTreeSet::new();
    meta.scan_file_urls(|digest, value| {
        if Digest::from_hex(digest).is_none() || split_pair(value).is_none() {
            bail!("invalid file URL record for digest {digest:?}");
        }
        digests.insert(digest.to_owned());
        Ok::<(), anyhow::Error>(())
    })
    .map_err(|err| anyhow::anyhow!("{err}"))
    .context("scan file URL references")?;
    meta.scan_metadata_records(|digest, value| {
        let Some((_url, metadata_digest, _source)) = split_triple(value) else {
            bail!("invalid PEP 658 metadata record for digest {digest:?}");
        };
        if Digest::from_hex(digest).is_none() {
            bail!("invalid PEP 658 wheel digest {digest:?}");
        }
        if Digest::from_hex(metadata_digest).is_none() {
            bail!("invalid PEP 658 metadata digest {metadata_digest:?}");
        }
        digests.insert(digest.to_owned());
        digests.insert(metadata_digest.to_owned());
        Ok::<(), anyhow::Error>(())
    })
    .map_err(|err| anyhow::anyhow!("{err}"))
    .context("scan PEP 658 references")?;
    meta.scan_upload_records(|key, bytes| {
        for digest in upload_digests(bytes).with_context(|| format!("read upload record {key}"))? {
            digests.insert(digest.as_str().to_owned());
        }
        Ok::<(), anyhow::Error>(())
    })
    .map_err(|err| anyhow::anyhow!("{err}"))
    .context("scan upload references")?;
    Ok(digests)
}

fn add_index_refs(refs: &mut CacheRefs, record: &CachedIndex) -> anyhow::Result<()> {
    for file in parse_detail(&record.body)?.files {
        let Some(sha256) = file.hashes.get("sha256") else {
            continue;
        };
        if Digest::from_hex(sha256).is_none() {
            bail!("cached page contains invalid sha256 digest {sha256:?}");
        }
        refs.files.insert(sha256.to_owned());
        if let CoreMetadata::Hashes(hashes) = file.core_metadata
            && let Some(metadata_digest) = hashes.get("sha256")
        {
            if Digest::from_hex(metadata_digest).is_none() {
                bail!("cached page contains invalid metadata digest {metadata_digest:?}");
            }
            refs.metadata_wheels.insert(sha256.to_owned());
        }
    }
    Ok(())
}

fn write_project_purge_counts(
    out: &mut dyn Write,
    action: &str,
    index: &str,
    project: &str,
    counts: ProjectCachePurgeCounts,
) -> anyhow::Result<()> {
    writeln!(
        out,
        "{action}\tproject\t{index}\t{project}\t{}\t{}\t{}\t{}",
        counts.index_pages, counts.project_records, counts.file_url_records, counts.metadata_records
    )?;
    Ok(())
}

fn upload_digests(bytes: &[u8]) -> Option<Vec<Digest>> {
    let upload: Uploaded = serde_json::from_slice(bytes).ok()?;
    let mut digests = Vec::new();
    let content_digest = upload.file.hashes.get("sha256")?;
    digests.push(Digest::from_hex(content_digest)?);
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

fn index_names(config: &Config) -> Vec<&str> {
    let mut names = config
        .indexes
        .iter()
        .map(|index| index.name.as_str())
        .collect::<Vec<_>>();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names
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

fn age_secs(now: i64, fetched_at: i64) -> u64 {
    now.saturating_sub(fetched_at).try_into().unwrap_or_default()
}

const fn is_stale(age: u64, ttl: i64) -> bool {
    ttl <= 0 || age >= ttl.cast_unsigned()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().try_into().unwrap_or(i64::MAX))
}
