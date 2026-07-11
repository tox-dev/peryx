use peryx_storage::meta::{DriverBatch, MetaError, MetaScanError, MetaStore};

use super::journal::JournalEntry;
use super::{OVERRIDE_PREFIX, UPLOAD_PREFIX, metadata_key, metadata_value, override_key, project_key, upload_key};

/// The PEP 658 metadata sibling recorded alongside a published file.
pub struct MetadataSibling<'a> {
    /// The artifact's own sha256, which keys the row.
    pub artifact_sha256: &'a str,
    /// Where the sibling came from; `uploaded` for a file published here.
    pub url: &'a str,
    /// The sibling's sha256, so a later fetch can verify it.
    pub metadata_sha256: &'a str,
    /// The index that owns it.
    pub source: &'a str,
}

/// Everything one published file writes to the store.
pub struct PublishedFile<'a> {
    /// The hosted index the file lands on.
    pub index: &'a str,
    /// The project's normalized name, which keys its rows.
    pub normalized: &'a str,
    /// The project's display name, as the uploader spelled it.
    pub display: &'a str,
    /// The distribution filename.
    pub filename: &'a str,
    /// The serialized file record served on the project's page.
    pub record: &'a [u8],
    /// The release the file belongs to, recorded in the journal entry.
    pub version: &'a str,
    /// The file's metadata sibling, when it has one.
    pub metadata: Option<MetadataSibling<'a>>,
}

/// Publish a file: its metadata sibling, its record, its project, and its journal entry, together.
///
/// One transaction, because these four rows are one fact. Committed separately, a crash between
/// the upload row and the journal entry leaves peryx serving a file forever that no replica will
/// ever receive: nothing reconciles the journal against the file tables at startup, and `fsck`
/// does not audit it. Being one transaction it is also one fsync rather than four.
///
/// Returns the journal serial the publication was recorded under.
///
/// # Errors
/// Returns a store error if the write, encode, or commit fails.
pub fn publish_file(meta: &MetaStore, file: &PublishedFile) -> Result<u64, MetaError> {
    let mut batch = DriverBatch::new();
    if let Some(sibling) = &file.metadata {
        batch.put(
            metadata_key(sibling.artifact_sha256),
            metadata_value(sibling.url, sibling.metadata_sha256, sibling.source).into_bytes(),
        );
    }
    batch.put(
        upload_key(file.index, file.normalized, file.filename),
        file.record.to_vec(),
    );
    batch.put(
        project_key(file.index, file.normalized),
        file.display.as_bytes().to_vec(),
    );
    meta.commit_driver_batch_journaled(
        &batch,
        &journal_bytes("add-file", file.normalized, Some(file.version), Some(file.filename)),
    )
}

/// Store an uploaded file's serialized record on a private index, keyed by
/// `{index}/{normalized}/{filename}` so each file is an independent entry (no read-modify-write
/// race between concurrent uploads).
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_upload(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    filename: &str,
    record: &[u8],
) -> Result<(), MetaError> {
    meta.put_driver_value(&upload_key(index, normalized, filename), record)
}

/// Promote a release onto `index`: its file records, its project, and its journal entry, together.
///
/// One transaction, for the same reason [`publish_file`] is: a promotion the journal never records
/// is invisible to every replica, and nothing reconciles that later.
///
/// Returns the journal serial the promotion was recorded under.
///
/// # Errors
/// Returns a store error if the write, encode, or commit fails.
pub fn promote_files(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    display: &str,
    records: &[(String, Vec<u8>)],
) -> Result<u64, MetaError> {
    let mut batch = DriverBatch::new();
    for (filename, record) in records {
        batch.put(upload_key(index, normalized, filename), record.clone());
    }
    batch.put(project_key(index, normalized), display.as_bytes().to_vec());
    meta.commit_driver_batch_journaled(&batch, &journal_bytes("promote", normalized, None, None))
}

/// Fetch one uploaded file record.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn get_upload(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    filename: &str,
) -> Result<Option<Vec<u8>>, MetaError> {
    meta.get_driver_value(&upload_key(index, normalized, filename))
}

/// List the `(filename, record)` pairs uploaded for `normalized` on `index`, sorted by filename.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn list_upload_entries(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
) -> Result<Vec<(String, Vec<u8>)>, MetaError> {
    let prefix = format!("{UPLOAD_PREFIX}{index}/{normalized}/");
    let mut entries = Vec::new();
    for key in meta.driver_prefix_keys(&prefix)? {
        if let (Some(filename), Some(record)) = (key.strip_prefix(&prefix), meta.get_driver_value(&key)?) {
            entries.push((filename.to_owned(), record));
        }
    }
    Ok(entries)
}

/// Delete one uploaded file record, returning whether it existed.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn delete_upload(meta: &MetaStore, index: &str, normalized: &str, filename: &str) -> Result<bool, MetaError> {
    meta.delete_driver_value(&upload_key(index, normalized, filename))
}

/// Visit raw upload records, keyed by `{index}/{normalized}/{filename}`.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_upload_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(UPLOAD_PREFIX)? {
        let (Some(logical), Some(record)) = (key.strip_prefix(UPLOAD_PREFIX), meta.get_driver_value(&key)?) else {
            continue;
        };
        visit(logical, &record).map_err(MetaScanError::Visit)?;
    }
    Ok(())
}

/// Record an override for a file served from a read-only layer: `kind` is `yanked` or
/// `hidden`. Keyed like uploads, by `{index}/{normalized}/{filename}`.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_override(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    filename: &str,
    kind: &str,
) -> Result<(), MetaError> {
    meta.put_driver_value(&override_key(index, normalized, filename), kind.as_bytes())
}

/// Remove a file's override, returning whether one existed.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn delete_override(meta: &MetaStore, index: &str, normalized: &str, filename: &str) -> Result<bool, MetaError> {
    meta.delete_driver_value(&override_key(index, normalized, filename))
}

/// List the `(filename, kind)` overrides recorded for `normalized` on `index`.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn list_overrides(meta: &MetaStore, index: &str, normalized: &str) -> Result<Vec<(String, String)>, MetaError> {
    let prefix = format!("{OVERRIDE_PREFIX}{index}/{normalized}/");
    let mut entries = Vec::new();
    for key in meta.driver_prefix_keys(&prefix)? {
        if let (Some(filename), Some(kind)) = (
            key.strip_prefix(&prefix),
            meta.get_driver_value(&key)?.and_then(|raw| String::from_utf8(raw).ok()),
        ) {
            entries.push((filename.to_owned(), kind));
        }
    }
    Ok(entries)
}

/// Visit raw override records, keyed by `{index}/{normalized}/{filename}`.
///
/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_override_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(OVERRIDE_PREFIX)? {
        let (Some(logical), Some(raw)) = (key.strip_prefix(OVERRIDE_PREFIX), meta.get_driver_value(&key)?) else {
            continue;
        };
        let Ok(kind) = String::from_utf8(raw) else {
            continue;
        };
        visit(logical, &kind).map_err(MetaScanError::Visit)?;
    }
    Ok(())
}

/// Serialize a journal entry for the journaled batch primitive. `serial` is a placeholder: the
/// store allocates the authoritative serial and returns it, so the value here is never read back.
fn journal_bytes(action: &str, project: &str, version: Option<&str>, filename: Option<&str>) -> Vec<u8> {
    serde_json::to_vec(&JournalEntry {
        serial: 0,
        action: action.to_owned(),
        project: project.to_owned(),
        version: version.map(str::to_owned),
        filename: filename.map(str::to_owned),
    })
    .expect("journal entry always serializes")
}
