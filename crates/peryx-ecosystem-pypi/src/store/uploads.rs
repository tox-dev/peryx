use peryx_storage::meta::{MetaError, MetaScanError, MetaStore};

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

/// A precondition's verdict on a key's current value, decided inside the write transaction.
///
/// `Commit` writes the staged rows; `Skip` leaves the key untouched as an idempotent no-op. A rejection
/// is the guard returning an error.
pub enum Guard {
    Commit,
    Skip,
}

/// What a per-file upload mutation does to one record inside the transaction.
pub enum UploadMutation {
    Keep,
    Replace(Vec<u8>),
    Delete,
}

/// Publish a file, but only if `guard` accepts the filename's current stored record.
///
/// Its metadata sibling, its record, its project, and its journal entry go in together, and the guard
/// runs in the same write transaction as those writes. One transaction, because these four rows are
/// one fact. Committed separately, a crash between
/// the upload row and the journal entry leaves peryx serving a file forever that no replica will
/// ever receive: nothing reconciles the journal against the file tables at startup, and `fsck`
/// does not audit it. Being one transaction it is also one fsync rather than four. The guard runs in
/// that transaction too, so a concurrent upload of the same name cannot slip between the duplicate
/// check and the publish and overwrite a record whose bytes a client already resolved.
///
/// `guard` sees the file's current record (`None` when unpublished) and returns [`Guard::Commit`] to
/// publish, [`Guard::Skip`] to treat it as an idempotent no-op, or an error to reject it. Returns
/// whether the file was written.
///
/// # Errors
/// Returns the guard's error, or a store error mapped into it, if the transaction fails.
pub fn publish_file_if<E: From<MetaError>>(
    meta: &MetaStore,
    file: &PublishedFile,
    guard: impl FnOnce(Option<&[u8]>) -> Result<Guard, E>,
) -> Result<bool, E> {
    let upload = upload_key(file.index, file.normalized, file.filename);
    meta.commit_driver_txn(|txn| match guard(txn.get(&upload)?.as_deref())? {
        Guard::Skip => Ok((false, None)),
        Guard::Commit => {
            if let Some(sibling) = &file.metadata {
                let value = metadata_value(sibling.url, sibling.metadata_sha256, sibling.source);
                txn.put(&metadata_key(sibling.artifact_sha256), value.as_bytes())?;
            }
            txn.put(&upload, file.record)?;
            txn.put(&project_key(file.index, file.normalized), file.display.as_bytes())?;
            let journal = journal_bytes("add-file", file.normalized, Some(file.version), Some(file.filename));
            Ok((true, Some(journal)))
        }
    })
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

/// Promote a release onto `index`, each target filename admitted only if `guard` accepts it.
///
/// Its file records, its project, and its journal entry go in together, and `guard` runs against each
/// target's current stored record inside that write transaction. One transaction, for the same reason
/// [`publish_file_if`] is: a promotion the journal never records
/// is invisible to every replica, and nothing reconciles that later; and the target existence check
/// runs in it, so a concurrent upload to the target cannot land between the check and the copy.
///
/// Each record is `(filename, token, bytes)`; `token` is opaque here and passed to `guard` to
/// compare against the existing target row. `guard` returns [`Guard::Commit`] to copy the file,
/// [`Guard::Skip`] to leave an identical target as it is, or an error to reject a conflict. Returns
/// how many files were written; the project row and journal entry are recorded only when at least one
/// was.
///
/// # Errors
/// Returns the guard's error, or a store error mapped into it, if the transaction fails.
pub fn promote_files_checked<E: From<MetaError>>(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    display: &str,
    records: &[(String, String, Vec<u8>)],
    guard: impl Fn(&str, &str, Option<&[u8]>) -> Result<Guard, E>,
) -> Result<usize, E> {
    meta.commit_driver_txn(|txn| {
        let mut written = 0;
        for (filename, token, record) in records {
            let key = upload_key(index, normalized, filename);
            match guard(filename, token, txn.get(&key)?.as_deref())? {
                Guard::Skip => {}
                Guard::Commit => {
                    txn.put(&key, record)?;
                    written += 1;
                }
            }
        }
        if written == 0 {
            return Ok((0, None));
        }
        txn.put(&project_key(index, normalized), display.as_bytes())?;
        Ok((written, Some(journal_bytes("promote", normalized, None, None))))
    })
}

/// Apply a per-file mutation to every uploaded record of `normalized` on `index`.
///
/// The listing and the writes share one transaction, so a concurrent upload cannot land between them
/// and be missed or resurrected. `mutate` sees each `(filename, record)` and returns
/// [`UploadMutation::Keep`] to leave it,
/// [`UploadMutation::Replace`] to rewrite it, or [`UploadMutation::Delete`] to remove it; an error
/// aborts the whole transaction unchanged. Returns how many records were rewritten or removed. The
/// mutation carries no journal entry: a yank or delete is local override state no replica reconciles.
///
/// # Errors
/// Returns the closure's error, or a store error mapped into it, if the transaction fails.
///
/// # Panics
/// Never in practice: every key comes from a prefix scan of `prefix`, so each carries it.
pub fn mutate_uploads<E: From<MetaError>>(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
    mut mutate: impl FnMut(&str, &[u8]) -> Result<UploadMutation, E>,
) -> Result<usize, E> {
    let prefix = format!("{UPLOAD_PREFIX}{index}/{normalized}/");
    meta.commit_driver_txn(|txn| {
        let mut changed = 0;
        for (key, record) in txn.prefix(&prefix)? {
            let filename = key
                .strip_prefix(&prefix)
                .expect("a key from the prefix scan carries the prefix");
            match mutate(filename, &record)? {
                UploadMutation::Keep => {}
                UploadMutation::Replace(bytes) => {
                    txn.put(&key, &bytes)?;
                    changed += 1;
                }
                UploadMutation::Delete => {
                    txn.remove(&key)?;
                    changed += 1;
                }
            }
        }
        Ok((changed, None))
    })
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
///
/// # Panics
/// Never in practice: a key the prefix scan just returned still has its value.
pub fn scan_upload_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    for key in meta.driver_prefix_keys(UPLOAD_PREFIX)? {
        let record = meta
            .get_driver_value(&key)?
            .expect("a key from the prefix scan still has its value");
        visit(&key[UPLOAD_PREFIX.len()..], &record).map_err(MetaScanError::Visit)?;
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
        if let Some(kind) = meta.get_driver_value(&key)?.and_then(|raw| String::from_utf8(raw).ok()) {
            visit(&key[OVERRIDE_PREFIX.len()..], &kind).map_err(MetaScanError::Visit)?;
        }
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

#[cfg(test)]
mod tests {
    use super::{Guard, MetaError, MetaStore, MetadataSibling, PublishedFile, override_key, upload_key};
    use crate::store::PypiStore as _;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn published() -> PublishedFile<'static> {
        PublishedFile {
            index: "hosted",
            normalized: "flask",
            display: "Flask",
            filename: "flask-1.0.whl",
            record: b"record",
            version: "1.0",
            metadata: Some(MetadataSibling {
                artifact_sha256: "artifact-sha",
                url: "uploaded",
                metadata_sha256: "metadata-sha",
                source: "hosted",
            }),
        }
    }

    #[test]
    fn test_publish_file_if_commit_writes_record_sibling_project_and_serial() {
        let (_dir, meta) = store();

        let wrote = meta
            .publish_file_if(&published(), |existing| {
                assert!(existing.is_none(), "a first publish sees no prior record");
                Ok::<_, MetaError>(Guard::Commit)
            })
            .unwrap();

        assert!(wrote);
        assert_eq!(
            meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
                .unwrap()
                .as_deref(),
            Some(b"record".as_slice())
        );
        assert!(
            meta.get_metadata("artifact-sha").unwrap().is_some(),
            "the sibling row is written"
        );
        assert_eq!(meta.get_project("hosted", "flask").unwrap().as_deref(), Some("Flask"));
        assert_eq!(meta.current_serial().unwrap(), 1, "the publish is journaled");
    }

    #[test]
    fn test_publish_file_if_commit_without_a_metadata_sibling_writes_no_sibling() {
        let (_dir, meta) = store();

        let wrote = meta
            .publish_file_if(
                &PublishedFile {
                    metadata: None,
                    ..published()
                },
                |_existing| Ok::<_, MetaError>(Guard::Commit),
            )
            .unwrap();

        assert!(wrote);
        assert!(
            meta.get_metadata("artifact-sha").unwrap().is_none(),
            "a file without metadata records no sibling row"
        );
        assert_eq!(
            meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
                .unwrap()
                .as_deref(),
            Some(b"record".as_slice())
        );
    }

    #[test]
    fn test_publish_file_if_skip_leaves_the_store_unchanged() {
        let (_dir, meta) = store();

        let wrote = meta
            .publish_file_if(&published(), |_existing| Ok::<_, MetaError>(Guard::Skip))
            .unwrap();

        assert!(!wrote);
        assert!(
            meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
                .unwrap()
                .is_none()
        );
        assert_eq!(meta.current_serial().unwrap(), 0, "a skipped publish records no serial");
    }

    #[test]
    fn test_publish_file_if_propagates_a_guard_rejection_without_writing() {
        let (_dir, meta) = store();

        let result = meta.publish_file_if(&published(), |_existing| {
            Err::<Guard, _>(MetaError::from(
                serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
            ))
        });

        assert!(result.is_err());
        assert!(
            meta.get_driver_value(&upload_key("hosted", "flask", "flask-1.0.whl"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_scan_upload_records_visits_each_row() {
        let (_dir, meta) = store();
        meta.put_upload("hosted", "flask", "flask-1.0.whl", b"upload").unwrap();
        let mut seen = Vec::new();
        meta.scan_upload_records(|key, value| {
            seen.push((key.to_owned(), value.to_vec()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![("hosted/flask/flask-1.0.whl".to_owned(), b"upload".to_vec())]
        );
    }

    #[test]
    fn test_scan_override_records_visits_valid_and_skips_non_utf8() {
        let (_dir, meta) = store();
        meta.put_override("hosted", "flask", "flask-1.0.whl", "hidden").unwrap();
        meta.put_driver_value(&override_key("hosted", "flask", "bad.whl"), &[0xff, 0xfe])
            .unwrap();
        let mut seen = Vec::new();
        meta.scan_override_records(|key, value| {
            seen.push((key.to_owned(), value.to_owned()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![("hosted/flask/flask-1.0.whl".to_owned(), "hidden".to_owned())]
        );
    }
}
