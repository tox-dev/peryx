use std::collections::BTreeMap;

use peryx_storage::meta::{DriverTxn, MetaError, MetaScanError, MetaStore, QuotaError, QuotaReservationRecord};
use uuid::Uuid;

use super::journal::JournalEntry;
use super::overrides::{FileOverride, OverrideMutation};
use super::{
    OVERRIDE_PREFIX, UPLOAD_PREFIX, announced_release_key, metadata_key, override_key, provenance_key,
    provenance_prefix, provenance_value, put_project_row, put_upload_row, record_str, remove_upload_row,
    scan_utf8_records, split_provenance_value, upload_key,
};
use crate::{distribution_python_tag, distribution_version_segment};

/// The PEP 658 metadata sibling recorded alongside a published file, extracted from the
/// distribution's own bytes at upload.
pub struct MetadataSibling<'a> {
    /// The sibling's sha256, which the page advertises and a reader verifies.
    pub metadata_sha256: &'a str,
    /// The sibling's byte length.
    pub size: u64,
}

/// The PEP 740 provenance blob published alongside a distribution that carried attestations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvenanceSibling<'a> {
    /// The provenance blob's own sha256, which serving and the blob reference both key on.
    pub provenance_sha256: &'a str,
    /// The provenance blob's byte length.
    pub size: u64,
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
    /// The artifact's sha256.
    pub artifact_sha256: &'a str,
    /// The artifact's byte length.
    pub artifact_size: u64,
    /// The serialized file record served on the project's page.
    pub record: &'a [u8],
    /// The release the file belongs to, recorded in the journal entry.
    pub version: &'a str,
    /// When the upload was submitted, as Unix seconds.
    pub submitted_at_unix: i64,
    /// The file's metadata sibling, when it has one.
    pub metadata: Option<MetadataSibling<'a>>,
    /// The file's PEP 740 provenance sibling, when the upload carried valid attestations.
    pub provenance: Option<ProvenanceSibling<'a>>,
    /// The capacity allocation to finalize with this file, when its upload is metered.
    pub quota: Option<&'a QuotaReservationRecord>,
}

/// Everything one release promotion writes to the store.
pub struct PromotedRelease<'a> {
    /// The hosted index the release is copied from, whose provenance bundles the promotion inherits.
    pub source: &'a str,
    /// The hosted index the release lands on.
    pub index: &'a str,
    /// The project's normalized name, which keys its rows.
    pub normalized: &'a str,
    /// The project's display name.
    pub display: &'a str,
    /// The serialized file records and their artifact digests.
    pub records: &'a [(String, String, Vec<u8>)],
    /// The artifact size for each digest known to the promotion.
    pub blob_sizes: &'a BTreeMap<String, u64>,
    /// The quota allocation held for each promoted filename. A file the guard skips releases its own
    /// allocation, so an idempotent re-promotion is accounted for as the no-op it is.
    pub reservations: &'a BTreeMap<String, Uuid>,
    /// When the promotion was submitted, as Unix seconds.
    pub submitted_at_unix: i64,
}

/// A precondition's verdict on a key's current value, decided inside the write transaction.
///
/// `Commit` writes the staged rows; `Skip` leaves the key untouched as an idempotent no-op. A rejection
/// is the guard returning an error.
pub enum Guard {
    Commit,
    Skip,
}

pub enum UploadMutation {
    Keep,
    Replace(Vec<u8>),
    Delete,
}

/// One publication as the store already holds it, for a precondition that has to weigh more than the
/// served record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedState<'a> {
    /// The served file record, `None` when the filename is unpublished on this index.
    pub record: Option<&'a [u8]>,
    /// The provenance bundle blob's sha256, when this publication carries one.
    pub provenance: Option<&'a str>,
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
/// `guard` sees the publication as the store holds it - its record and its provenance bundle, both
/// `None` when unpublished - and returns [`Guard::Commit`] to publish, [`Guard::Skip`] to treat it as
/// an idempotent no-op, or an error to reject it. Returns whether the file was written.
///
/// # Errors
/// Returns the guard's error, or a store error mapped into it, if the transaction fails.
pub fn publish_file_if<E: From<MetaError>>(
    meta: &MetaStore,
    outbox: bool,
    file: &PublishedFile,
    guard: impl FnOnce(PublishedState<'_>) -> Result<Guard, E>,
) -> Result<bool, E> {
    let Some(reservation) = file.quota else {
        return meta.commit_driver_txn(|txn| publish_file_in_txn(txn, outbox, file, guard, None));
    };
    meta.commit_driver_txn_with_quota_if(
        reservation.id,
        |stored| *stored,
        |txn| publish_file_in_txn(txn, outbox, file, guard, None).map_err(PublishError::Body),
    )
    .map_err(map_publish_error)
}

pub fn publish_file_with_commit_if<E: From<MetaError>>(
    meta: &MetaStore,
    outbox: bool,
    file: &PublishedFile,
    webhook: Option<peryx_storage::meta::WebhookEventIntent>,
    guard: impl FnOnce(PublishedState<'_>) -> Result<Guard, E>,
) -> Result<peryx_storage::meta::DriverCommit<bool>, E> {
    let Some(reservation) = file.quota else {
        return meta.commit_driver_txn_with_commit(|txn| publish_file_in_txn(txn, outbox, file, guard, webhook));
    };
    meta.commit_driver_txn_with_quota_if_commit(
        reservation.id,
        |stored| *stored,
        |txn| publish_file_in_txn(txn, outbox, file, guard, webhook).map_err(PublishError::Body),
    )
    .map_err(map_publish_error)
}

fn map_publish_error<E: From<MetaError>>(err: PublishError<E>) -> E {
    match err {
        PublishError::Body(err) => err,
        PublishError::Quota(QuotaError::Store(err)) => err.into(),
        PublishError::Quota(err) => MetaError::DriverPrecondition(err.to_string()).into(),
    }
}

enum PublishError<E> {
    Body(E),
    Quota(QuotaError),
}

impl<E> From<MetaError> for PublishError<E> {
    fn from(err: MetaError) -> Self {
        Self::Quota(err.into())
    }
}

impl<E> From<QuotaError> for PublishError<E> {
    fn from(err: QuotaError) -> Self {
        Self::Quota(err)
    }
}

pub fn publish_file_in_txn<E: From<MetaError>>(
    txn: &mut DriverTxn,
    outbox: bool,
    file: &PublishedFile,
    guard: impl FnOnce(PublishedState<'_>) -> Result<Guard, E>,
    webhook: Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<(bool, Vec<Vec<u8>>), E> {
    let upload = upload_key(file.index, file.normalized, file.filename);
    let provenance = provenance_key(file.index, file.normalized, file.artifact_sha256, file.filename);
    let record = txn.get(&upload)?;
    let stored_provenance = txn
        .get(&provenance)?
        .map(|raw| {
            let value = record_str(&provenance, raw)?;
            split_provenance_value(&provenance, &value).map(|(digest, _size)| digest.to_owned())
        })
        .transpose()?;
    match guard(PublishedState {
        record: record.as_deref(),
        provenance: stored_provenance.as_deref(),
    })? {
        Guard::Skip => Ok((false, Vec::new())),
        Guard::Commit => {
            txn.touch_policy_inputs(file.index);
            if let Some(webhook) = webhook {
                txn.enqueue_webhook_event(webhook);
            }
            txn.reference_blob(file.artifact_sha256, file.artifact_size);
            if let Some(sibling) = &file.metadata {
                txn.put(&metadata_key(file.artifact_sha256), sibling.metadata_sha256.as_bytes())?;
                txn.reference_blob(sibling.metadata_sha256, sibling.size);
            }
            if let Some(sibling) = &file.provenance {
                let value = provenance_value(sibling.provenance_sha256, sibling.size);
                txn.put(&provenance, value.as_bytes())?;
                txn.reference_blob(sibling.provenance_sha256, sibling.size);
            }
            put_upload_row(txn, file.index, file.normalized, file.filename, file.record)?;
            put_project_row(txn, file.index, file.normalized, file.display)?;
            let mut journal = Vec::new();
            if outbox {
                let target = JournalTarget::of(file.index, file.normalized, file.submitted_at_unix);
                journal.extend(announce_release(txn, &target, Some(file.version))?);
                journal.push(journal_bytes(
                    "add-file",
                    file.normalized,
                    Some(file.version),
                    Some(file.filename),
                    Some(distribution_python_tag(file.filename)),
                    file.submitted_at_unix,
                ));
            }
            Ok((true, journal))
        }
    }
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
    meta.commit_driver_txn(|txn| put_upload_row(txn, index, normalized, filename, record).map(|()| ((), Vec::new())))
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
/// compare against the existing target row. A token present in `blob_sizes` also records that blob
/// on the promotion serial. `guard` returns [`Guard::Commit`] to copy the file, [`Guard::Skip`] to
/// leave an identical target as it is, or an error to reject a conflict. Returns how many files were
/// written; the project row and journal entry are recorded only when at least one was.
///
/// # Errors
/// Returns the guard's error, or a store error mapped into it, if the transaction fails.
pub fn promote_files_checked<E: From<MetaError>>(
    meta: &MetaStore,
    outbox: bool,
    release: &PromotedRelease<'_>,
    guard: impl Fn(&str, &str, Option<&[u8]>) -> Result<Guard, E>,
) -> Result<usize, E> {
    let held: Vec<Uuid> = release.reservations.values().copied().collect();
    meta.commit_driver_txn_with_quotas::<_, PublishError<E>>(
        &held,
        |(_, committed): &(usize, Vec<Uuid>)| committed.clone(),
        |txn| {
            let mut written = 0;
            let mut committed = Vec::new();
            let mut journal = Vec::new();
            for (filename, token, record) in release.records {
                let key = upload_key(release.index, release.normalized, filename);
                match guard(filename, token, txn.get(&key)?.as_deref()).map_err(PublishError::Body)? {
                    Guard::Skip => {}
                    Guard::Commit => {
                        committed.extend(release.reservations.get(filename).copied());
                        txn.touch_policy_inputs(release.index);
                        put_upload_row(txn, release.index, release.normalized, filename, record)?;
                        if let Some(size) = release.blob_sizes.get(token) {
                            txn.reference_blob(token, *size);
                        }
                        copy_provenance_in_txn(txn, release, filename, token)?;
                        written += 1;
                        if outbox {
                            journal.extend(promoted_file_journal(txn, release, filename, record)?);
                        }
                    }
                }
            }
            if written == 0 {
                return Ok(((0, Vec::new()), Vec::new()));
            }
            put_project_row(txn, release.index, release.normalized, release.display)?;
            Ok(((written, committed), journal))
        },
    )
    .map(|(written, _)| written)
    .map_err(map_publish_error)
}

/// Carry a promoted publication's provenance bundle onto the target index.
///
/// The bundle belongs to the publication, so the target gets its own row and its own blob reference:
/// deleting either publication leaves the other's bundle readable, and the blob is reclaimable only
/// once the last publication releases it. A source row too damaged to name its blob fails the
/// promotion rather than publishing a target that advertises provenance nothing backs.
fn copy_provenance_in_txn(
    txn: &mut DriverTxn,
    release: &PromotedRelease<'_>,
    filename: &str,
    artifact_sha256: &str,
) -> Result<(), MetaError> {
    let source = provenance_key(release.source, release.normalized, artifact_sha256, filename);
    let Some(raw) = txn.get(&source)? else {
        return Ok(());
    };
    let value = record_str(&source, raw.clone())?;
    let (bundle, size) = split_provenance_value(&source, &value)?;
    txn.reference_blob(bundle, size);
    txn.put(
        &provenance_key(release.index, release.normalized, artifact_sha256, filename),
        &raw,
    )
}

/// Apply a per-file mutation to every uploaded record of `normalized` on `index`, journaling
/// `action` for each record it changes.
///
/// The listing, the writes, and the journal entries share one transaction, so a concurrent upload
/// cannot land between them and be missed or resurrected, and a crash cannot keep a row while losing
/// its entry. `mutate` sees each `(filename, record)` and returns [`UploadMutation::Keep`] to leave
/// it, [`UploadMutation::Replace`] to rewrite it, or [`UploadMutation::Delete`] to remove it; an
/// error aborts the whole transaction unchanged. Every rewritten or removed record records one
/// `action` entry against its filename - `yank`, `unyank`, or `delete-file`, the mutation the caller
/// knows it applied but the opaque record bytes cannot reveal - so a replica replays exactly the
/// files that changed. Returns how many records were rewritten or removed.
///
/// # Errors
/// Returns the closure's error, or a store error mapped into it, if the transaction fails.
///
pub fn mutate_uploads<E: From<MetaError>>(
    meta: &MetaStore,
    outbox: bool,
    index: &str,
    normalized: &str,
    action: &str,
    submitted_at_unix: i64,
    mut mutate: impl FnMut(&str, &[u8]) -> Result<UploadMutation, E>,
) -> Result<usize, E> {
    let prefix = format!("{UPLOAD_PREFIX}{index}/{normalized}/");
    meta.commit_driver_txn(|txn| {
        let mut changed = 0;
        let mut journal = Vec::new();
        for (key, record) in txn.prefix(&prefix)? {
            let filename = &key[prefix.len()..];
            match mutate(filename, &record)? {
                UploadMutation::Keep => continue,
                UploadMutation::Replace(bytes) => put_upload_row(txn, index, normalized, filename, &bytes)?,
                UploadMutation::Delete => remove_upload_row(txn, index, normalized, filename, &record)?,
            }
            txn.touch_policy_inputs(index);
            changed += 1;
            journal.extend(journal_entries(outbox, || {
                journal_bytes(
                    action,
                    normalized,
                    journal_version(filename, &record).as_deref(),
                    Some(filename),
                    None,
                    submitted_at_unix,
                )
            }));
        }
        Ok((changed, journal))
    })
}

#[derive(Clone, Copy)]
pub struct UploadMutationPlan<'a> {
    pub outbox: bool,
    pub index: &'a str,
    pub normalized: &'a str,
    pub action: &'a str,
    pub submitted_at_unix: i64,
    pub override_filenames: &'a [String],
    pub override_mutation: OverrideMutation<'a>,
}

pub fn mutate_uploads_and_overrides<E: From<MetaError>>(
    meta: &MetaStore,
    plan: UploadMutationPlan<'_>,
    guard: impl Fn() -> Result<(), E>,
    mut mutate: impl FnMut(&str, &[u8]) -> Result<Option<Vec<u8>>, E>,
    webhook: impl FnOnce(usize) -> Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<usize, E> {
    let prefix = format!("{UPLOAD_PREFIX}{}/{}/", plan.index, plan.normalized);
    meta.commit_driver_txn(|txn| {
        let mut changed = 0;
        let mut journal = Vec::new();
        for (key, record) in txn.prefix(&prefix)? {
            let filename = &key[prefix.len()..];
            let Some(bytes) = mutate(filename, &record)? else {
                continue;
            };
            guard()?;
            txn.touch_policy_inputs(plan.index);
            put_upload_row(txn, plan.index, plan.normalized, filename, &bytes)?;
            changed += 1;
            journal.extend(journal_entries(plan.outbox, || {
                journal_bytes(
                    plan.action,
                    plan.normalized,
                    journal_version(filename, &record).as_deref(),
                    Some(filename),
                    None,
                    plan.submitted_at_unix,
                )
            }));
        }
        for filename in plan.override_filenames {
            guard()?;
            let key = override_key(plan.index, plan.normalized, filename);
            let Some(override_action) = apply_override(txn, &key, plan.override_mutation)? else {
                continue;
            };
            txn.touch_policy_inputs(plan.index);
            changed += 1;
            journal.extend(journal_entries(plan.outbox, || {
                journal_bytes(
                    override_action,
                    plan.normalized,
                    distribution_version_segment(filename),
                    Some(filename),
                    None,
                    plan.submitted_at_unix,
                )
            }));
        }
        if changed > 0
            && let Some(webhook) = webhook(changed)
        {
            txn.enqueue_webhook_event(webhook);
        }
        Ok((changed, journal))
    })
}

/// The stored record for one hosted file, for a caller that knows the filename and would otherwise
/// list a whole project to find it.
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

/// # Errors
/// Returns a store error if the read fails.
pub fn list_upload_entries(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
) -> Result<Vec<(String, Vec<u8>)>, MetaError> {
    let prefix = format!("{UPLOAD_PREFIX}{index}/{normalized}/");
    let mut entries = Vec::new();
    meta.visit_driver_prefix(&prefix, |key, record| {
        entries.push((key[prefix.len()..].to_owned(), record.to_vec()));
    })?;
    Ok(entries)
}

/// Drop the provenance bundle a deleted publication held, so the blob's last reference goes with the
/// publication that claimed it and the orphan collector can reclaim the bytes.
///
/// The row is keyed by the artifact digest the record carries, which only the record itself names, so
/// the release matches on the filename segment instead of decoding opaque record bytes.
fn release_provenance_in_txn(
    txn: &mut DriverTxn,
    index: &str,
    normalized: &str,
    filename: &str,
) -> Result<(), MetaError> {
    let published = format!("/{filename}");
    txn.prefix(&provenance_prefix(index, normalized))?
        .into_iter()
        .filter(|(key, _)| key.ends_with(&published))
        .try_for_each(|(key, _)| txn.remove(&key).map(|_| ()))
}

/// # Errors
/// Returns a store error if the write fails.
pub fn delete_upload(
    meta: &MetaStore,
    outbox: bool,
    index: &str,
    normalized: &str,
    filename: &str,
    submitted_at_unix: i64,
) -> Result<bool, MetaError> {
    meta.commit_driver_txn(|txn| {
        let key = upload_key(index, normalized, filename);
        if let Some(record) = txn.get(&key)? {
            txn.touch_policy_inputs(index);
            remove_upload_row(txn, index, normalized, filename, &record)?;
            release_provenance_in_txn(txn, index, normalized, filename)?;
            Ok((
                true,
                journal_entries(outbox, || {
                    journal_bytes(
                        "delete-file",
                        normalized,
                        journal_version(filename, &record).as_deref(),
                        Some(filename),
                        None,
                        submitted_at_unix,
                    )
                }),
            ))
        } else {
            Ok((false, Vec::new()))
        }
    })
}

/// # Errors
/// Returns a scan error if the store read fails or either callback returns an error.
///
pub fn scan_upload_records<E>(
    meta: &MetaStore,
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    let mut error = None;
    meta.visit_driver_prefix(UPLOAD_PREFIX, |key, record| {
        if error.is_none() {
            error = visit(&key[UPLOAD_PREFIX.len()..], record).err();
        }
    })?;
    if let Some(err) = error {
        return Err(MetaScanError::Visit(err));
    }
    Ok(())
}

/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_upload_policy_snapshot<E>(
    meta: &MetaStore,
    index: &str,
    start: impl FnOnce(peryx_storage::meta::PolicyInputGeneration) -> Result<(), E>,
    mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    let prefix = format!("{UPLOAD_PREFIX}{index}/");
    meta.visit_driver_policy_snapshot(&prefix, index, start, |key, value| visit(&key[prefix.len()..], value))
}

/// Apply one field change to a file's override record, keyed like uploads by
/// `{index}/{normalized}/{filename}`, and return whether the record moved.
///
/// The record and its `hide`, `restore`, `yank`, or `unyank` journal entry commit in one
/// transaction, so a replica observes the change the way it observes a publish, and nothing is left
/// to reconcile after a crash. Re-recording a value the file already carries is a no-op that
/// allocates no serial, and a record left imposing nothing is removed rather than stored.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn set_override(
    meta: &MetaStore,
    outbox: bool,
    index: &str,
    normalized: &str,
    filename: &str,
    mutation: OverrideMutation<'_>,
    submitted_at_unix: i64,
) -> Result<bool, MetaError> {
    let key = override_key(index, normalized, filename);
    meta.commit_driver_txn(|txn| {
        let Some(action) = apply_override(txn, &key, mutation)? else {
            return Ok((false, Vec::new()));
        };
        txn.touch_policy_inputs(index);
        Ok((
            true,
            journal_entries(outbox, || {
                journal_bytes(
                    action,
                    normalized,
                    distribution_version_segment(filename),
                    Some(filename),
                    None,
                    submitted_at_unix,
                )
            }),
        ))
    })
}

/// Read, mutate, and write back one override record inside `txn`, returning the journal action when
/// the record moved.
///
/// A mutation moves one field and carries the other across, so an unreadable record cannot be
/// treated as absent: substituting a default silently drops whatever hide or yank the damaged row
/// held, and commits a journal entry telling every replica the file moved from a state it never
/// occupied. Corruption stops the write here and `fsck` names the row.
fn apply_override(
    txn: &mut DriverTxn,
    key: &str,
    mutation: OverrideMutation<'_>,
) -> Result<Option<&'static str>, MetaError> {
    let mut record = txn
        .get(key)?
        .map(|raw| FileOverride::decode(key, &record_str(key, raw)?))
        .transpose()?
        .unwrap_or_default();
    let Some(action) = mutation.apply(&mut record) else {
        return Ok(None);
    };
    if record.is_empty() {
        txn.remove(key)?;
    } else {
        txn.put(key, record.encode().as_bytes())?;
    }
    Ok(Some(action))
}

/// Every override an operator recorded over one project's files, keyed by filename.
///
/// A damaged row fails the read rather than dropping out of the map. An override is a withdrawal an
/// operator imposed, so serving a page that silently omits one serves a file that was administratively
/// deleted or yanked; an error on the page is the safer answer.
///
/// # Errors
/// Returns a store error if the read fails or a stored record does not decode.
pub fn list_overrides(
    meta: &MetaStore,
    index: &str,
    normalized: &str,
) -> Result<BTreeMap<String, FileOverride>, MetaError> {
    let prefix = format!("{OVERRIDE_PREFIX}{index}/{normalized}/");
    let mut entries = BTreeMap::new();
    meta.scan_driver_prefix(&prefix, |key, raw| {
        let record = FileOverride::decode(key, &record_str(key, raw.to_vec())?)?;
        entries.insert(key[prefix.len()..].to_owned(), record);
        Ok::<(), MetaError>(())
    })?;
    Ok(entries)
}

/// # Errors
/// Returns a scan error if the store read fails or the visitor returns an error.
pub fn scan_override_records<E>(
    meta: &MetaStore,
    visit: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<(), MetaScanError<E>> {
    scan_utf8_records(meta, OVERRIDE_PREFIX, visit)
}

/// Serialize a journal entry for the journaled batch primitive. `serial` is a placeholder: the
/// store allocates the authoritative serial and returns it, so the value here is never read back.
fn journal_bytes(
    action: &str,
    project: &str,
    version: Option<&str>,
    filename: Option<&str>,
    python: Option<&str>,
    submitted_at_unix: i64,
) -> Vec<u8> {
    serde_json::to_vec(&JournalEntry {
        serial: 0,
        submitted_at_unix,
        action: action.to_owned(),
        project: project.to_owned(),
        version: version.map(str::to_owned),
        filename: filename.map(str::to_owned),
        python: python.map(str::to_owned),
    })
    .expect("journal entry always serializes")
}

/// The index, project, and mutation time every journal entry for one publication shares.
struct JournalTarget<'a> {
    index: &'a str,
    normalized: &'a str,
    submitted_at_unix: i64,
}

impl<'a> JournalTarget<'a> {
    const fn of(index: &'a str, normalized: &'a str, submitted_at_unix: i64) -> Self {
        Self {
            index,
            normalized,
            submitted_at_unix,
        }
    }
}

/// The journal entries one promoted file contributes, in the order a client must read them.
fn promoted_file_journal(
    txn: &mut DriverTxn,
    release: &PromotedRelease<'_>,
    filename: &str,
    record: &[u8],
) -> Result<Vec<Vec<u8>>, MetaError> {
    let version = journal_version(filename, record);
    let target = JournalTarget::of(release.index, release.normalized, release.submitted_at_unix);
    let mut entries: Vec<Vec<u8>> = announce_release(txn, &target, version.as_deref())?
        .into_iter()
        .collect();
    entries.push(journal_bytes(
        "add-file",
        release.normalized,
        version.as_deref(),
        Some(filename),
        Some(distribution_python_tag(filename)),
        release.submitted_at_unix,
    ));
    Ok(entries)
}

/// Announce a release in the changelog the first time one of its files is journaled, and report the
/// `new-release` entry that has to precede the file's own.
///
/// Warehouse emits `new release` when it creates the `Release` row, which happens once per version
/// and before the file entry, so a mirror client creates the release and then attaches files to it.
/// peryx has no release row to hang that on, so it records that a release has been announced and
/// emits the event only when that record is absent.
///
/// The row tracks the announcement, not the release. An import journals nothing and so leaves the row
/// unset, and the first file that does reach the journal still announces the version rather than
/// attaching to a release no reader was ever told about. Nothing removes the row: deleting every file
/// leaves the release announced, matching Warehouse, where deleting files leaves the `Release` row
/// standing and a client keeps the release it already created.
///
/// The key is the version as written, where Warehouse keys on `canonicalize_version`, so a project
/// publishing `1.0` and `1.0.0` announces each spelling although `version_key` groups them into one
/// release on the detail page. That errs toward announcing twice, never toward not announcing: a
/// client that hears about a release it already has re-reads a page, while one that never hears about
/// it attaches files to a release it has not created.
fn announce_release(
    txn: &mut DriverTxn,
    target: &JournalTarget<'_>,
    version: Option<&str>,
) -> Result<Option<Vec<u8>>, MetaError> {
    let Some(version) = version else {
        return Ok(None);
    };
    let key = announced_release_key(target.index, target.normalized, version);
    if txn.get(&key)?.is_some() {
        return Ok(None);
    }
    txn.put(&key, version.as_bytes())?;
    Ok(Some(journal_bytes(
        "new-release",
        target.normalized,
        Some(version),
        None,
        None,
        target.submitted_at_unix,
    )))
}

fn journal_entries(outbox: bool, payload: impl FnOnce() -> Vec<u8>) -> Vec<Vec<u8>> {
    if outbox { vec![payload()] } else { Vec::new() }
}

fn journal_version(filename: &str, record: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(record)
        .ok()
        .and_then(|value| value["version"].as_str().map(str::to_owned))
        .or_else(|| distribution_version_segment(filename).map(str::to_owned))
}

#[cfg(test)]
#[path = "../../tests/unit/store/uploads/tests.rs"]
mod tests;
