//! Blobs and manifests are content-addressed, so they share the global
//! [`BlobStorage`](peryx_storage::blob::BlobStorage)/manifest
//! namespace and dedupe across proxies. Tags are mutable per proxy, so a tag key carries the index
//! route and the upstream repository to keep two registries' identically-named repos apart. The
//! [`MetaStore`] never interprets these keys; the driver owns the whole layout.

use std::collections::BTreeSet;

pub use peryx_core::TrashInfo;
use peryx_core::TrashRecord;
use peryx_ha::{ArtifactOrigin, ArtifactPlacement, ArtifactSource};
use peryx_storage::meta::{DriverTxn, MetaError, MetaStore};
use serde::{Deserialize, Serialize};

use crate::outbox::{self, OciMutation};

/// The driver-KV prefix every manifest is keyed under, its digest following.
mod descriptors;
pub use descriptors::{blob_digest, linux_amd64_child, manifest_descriptors, referenced_blob_digests};
mod fsck;
pub use fsck::fsck_metadata;

const MANIFEST_PREFIX: &str = "oci\u{0}m\u{0}";
const TAG_PREFIX: &str = "oci\u{0}t\u{0}";
const REFERRER_PREFIX: &str = "oci\u{0}r\u{0}";
const REFERRER_PAGE_PREFIX: &str = "oci\u{0}rp\u{0}";
const MEMBERSHIP_PREFIX: &str = "oci\u{0}mm\u{0}";
const BLOB_MEMBERSHIP_PREFIX: &str = "oci\u{0}bm\u{0}";
const MANIFEST_TRASH_PREFIX: &str = "oci\u{0}mt\u{0}";
const TAG_TRASH_PREFIX: &str = "oci\u{0}tt\u{0}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestTrash {
    #[serde(flatten)]
    info: TrashInfo,
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TagTrash {
    digest: String,
    #[serde(flatten)]
    info: TrashInfo,
}

/// The result of restoring one trashed tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreTagOutcome {
    Missing,
    Restored { digest: String },
}

/// The result of restoring one trashed digest and its unclaimed tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreManifestOutcome {
    Missing,
    Restored {
        restored: Vec<String>,
        conflicts: Vec<String>,
    },
}

/// Where an OCI content object came from, mapped once into the neutral [`ArtifactSource`] so no
/// neutral code decides an OCI object's origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OciArtifactOrigin {
    /// Pushed into a hosted registry on this instance.
    Pushed,
    /// Mirrored from an upstream registry through the pull-through cache.
    Mirrored,
}

impl ArtifactOrigin for OciArtifactOrigin {
    fn artifact_source(&self) -> ArtifactSource {
        match self {
            Self::Pushed => ArtifactSource::Hosted,
            Self::Mirrored => ArtifactSource::Proxy,
        }
    }
}

/// A stored manifest: its media type and the exact bytes whose digest addresses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl Manifest {
    fn encode(&self) -> Vec<u8> {
        let media_type = self.media_type.as_bytes();
        let mut out = Vec::with_capacity(2 + media_type.len() + self.bytes.len());
        out.extend_from_slice(&u16::try_from(media_type.len()).unwrap_or(u16::MAX).to_be_bytes());
        out.extend_from_slice(media_type);
        out.extend_from_slice(&self.bytes);
        out
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        let (length, rest) = raw.split_first_chunk::<2>()?;
        let length = usize::from(u16::from_be_bytes(*length));
        let (media_type, bytes) = rest.split_at_checked(length)?;
        Some(Self {
            media_type: String::from_utf8(media_type.to_vec()).ok()?,
            bytes: bytes.to_vec(),
        })
    }
}

fn manifest_key(digest: &str) -> String {
    format!("{MANIFEST_PREFIX}{digest}")
}

fn tag_key(index: &str, repo: &str, tag: &str) -> String {
    format!("{TAG_PREFIX}{index}\u{0}{repo}\u{0}{tag}")
}

fn tag_prefix(index: &str, repo: &str) -> String {
    format!("{TAG_PREFIX}{index}\u{0}{repo}\u{0}")
}

fn manifest_trash_key(index: &str, repo: &str, digest: &str) -> String {
    format!("{MANIFEST_TRASH_PREFIX}{index}\u{0}{repo}\u{0}{digest}")
}

fn tag_trash_key(index: &str, repo: &str, tag: &str) -> String {
    format!("{TAG_TRASH_PREFIX}{index}\u{0}{repo}\u{0}{tag}")
}

fn tag_trash_prefix(index: &str, repo: &str) -> String {
    format!("{TAG_TRASH_PREFIX}{index}\u{0}{repo}\u{0}")
}

/// Store a manifest and record it as one `(index, repo)` serves: its own digest, and - for an image
/// index or manifest list - each child it names. A by-digest read authorizes against this per-repository
/// membership, not the digest's presence in the global content store the bytes dedupe into, so a
/// manifest one repository cached is not readable by digest under another.
///
/// # Errors
/// Returns a store error if a write fails.
pub fn record_manifest(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    digest: &str,
    manifest: &Manifest,
) -> Result<(), MetaError> {
    meta.commit_driver_txn(|txn| {
        record_manifest_txn(txn, index, repo, digest, manifest)?;
        Ok(((), Vec::new()))
    })
}

/// Stage a manifest and its `(index, repo)` memberships inside an open transaction, so a caller can
/// publish it atomically with a quota-reservation commit.
///
/// # Errors
/// Returns a store error if a write fails.
fn record_manifest_txn(
    txn: &mut DriverTxn,
    index: &str,
    repo: &str,
    digest: &str,
    manifest: &Manifest,
) -> Result<bool, MetaError> {
    txn.put(&manifest_key(digest), &manifest.encode())?;
    let inserted = txn.upsert(&membership_key(index, repo, digest), &[])?;
    let (children, blobs) = manifest_descriptors(&manifest.bytes);
    for child in children {
        txn.put(&membership_key(index, repo, &child), &[])?;
    }
    for blob in blobs {
        txn.put(&blob_membership_key(index, repo, &blob), &[])?;
    }
    Ok(inserted)
}

fn membership_key(index: &str, repo: &str, digest: &str) -> String {
    format!("{MEMBERSHIP_PREFIX}{index}\u{0}{repo}\u{0}{digest}")
}

/// Record where an OCI content object came from and whether its verified bytes are local, so a later
/// read resolves its neutral placement from the index without probing the content store.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn record_content_placement(
    meta: &MetaStore,
    digest: &str,
    origin: OciArtifactOrigin,
    present: bool,
) -> Result<(), MetaError> {
    meta.put_artifact_placement(digest, &ArtifactPlacement::record(origin.artifact_source(), present))
}

/// Read local availability without creating the optional placement table.
pub fn content_available_locally(meta: &MetaStore, digest: &str) -> Result<bool, MetaError> {
    Ok(meta
        .get_artifact_placement(digest)?
        .is_some_and(|placement| placement.availability.is_local()))
}

/// # Errors
/// Returns a store error if the read fails.
pub fn get_manifest(meta: &MetaStore, digest: &str) -> Result<Option<Manifest>, MetaError> {
    Ok(meta
        .get_driver_value(&manifest_key(digest))?
        .and_then(|raw| Manifest::decode(&raw)))
}

/// # Errors
/// Returns a store error if the read fails.
pub fn manifest_is_member(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<bool, MetaError> {
    Ok(meta.get_driver_value(&membership_key(index, repo, digest))?.is_some())
}

/// # Errors
/// Returns a store error if the write fails.
pub fn record_blob_membership(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<(), MetaError> {
    meta.put_driver_value(&blob_membership_key(index, repo, digest), &[])
}

/// # Errors
/// Returns a store error if the read fails.
pub fn blob_is_member(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<bool, MetaError> {
    Ok(meta
        .get_driver_value(&blob_membership_key(index, repo, digest))?
        .is_some())
}

pub fn blob_membership_key(index: &str, repo: &str, digest: &str) -> String {
    format!("{BLOB_MEMBERSHIP_PREFIX}{index}\u{0}{repo}\u{0}{digest}")
}

#[must_use]
pub fn is_blob_membership_key(key: &str) -> bool {
    key.starts_with(BLOB_MEMBERSHIP_PREFIX)
}

/// # Errors
/// Returns a store error if the write fails.
pub fn put_tag(meta: &MetaStore, index: &str, repo: &str, tag: &str, digest: &str) -> Result<bool, MetaError> {
    meta.commit_driver_txn(|txn| Ok((put_tag_txn(txn, index, repo, tag, digest)?, Vec::new())))
}

/// Point `tag` at `digest` inside an open transaction, reporting whether its target changed,
/// so a caller can publish it atomically with a quota-reservation commit.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_tag_txn(txn: &mut DriverTxn, index: &str, repo: &str, tag: &str, digest: &str) -> Result<bool, MetaError> {
    let key = tag_key(index, repo, tag);
    let changed = txn.get(&key)?.as_deref() != Some(digest.as_bytes());
    txn.put(&key, digest.as_bytes())?;
    Ok(changed)
}

/// Publish a pushed manifest and make its explicit reference live in the same transaction. A digest
/// push restores that digest without reviving old tags; a tag push additionally supersedes any trash
/// entry for that tag.
///
/// # Errors
/// Returns a store error if a read or write fails.
pub fn publish_manifest_txn(
    txn: &mut DriverTxn,
    index: &str,
    repo: &str,
    digest: &str,
    manifest: &Manifest,
    tag: Option<&str>,
) -> Result<ManifestPublication, MetaError> {
    let inserted = record_manifest_txn(txn, index, repo, digest, manifest)?;
    let restored_manifest = txn.remove(&manifest_trash_key(index, repo, digest))?;
    let Some(tag) = tag else {
        return Ok(ManifestPublication {
            changed: inserted || restored_manifest,
            allocated: inserted,
        });
    };
    let restored_tag = txn.remove(&tag_trash_key(index, repo, tag))?;
    let allocated = txn.get(&tag_key(index, repo, tag))?.as_deref() != Some(digest.as_bytes());
    let changed_tag = put_tag_txn(txn, index, repo, tag, digest)?;
    Ok(ManifestPublication {
        changed: inserted || restored_manifest || restored_tag || changed_tag,
        allocated,
    })
}

pub struct ManifestPublication {
    pub changed: bool,
    pub allocated: bool,
}

/// # Errors
/// Returns a store error if the read fails.
pub fn get_tag(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<Option<String>, MetaError> {
    Ok(meta
        .get_driver_value(&tag_key(index, repo, tag))?
        .and_then(|raw| String::from_utf8(raw).ok()))
}

/// # Errors
/// Returns a store error if the read fails.
pub fn manifest_is_trashed(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<bool, MetaError> {
    Ok(meta
        .get_driver_value(&manifest_trash_key(index, repo, digest))?
        .is_some())
}

/// # Errors
/// Returns a store error if the read fails.
pub fn tag_is_trashed(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<bool, MetaError> {
    Ok(
        get_tag(meta, index, repo, tag)?.is_none()
            && meta.get_driver_value(&tag_trash_key(index, repo, tag))?.is_some(),
    )
}

/// # Errors
/// Returns a store error if the read fails.
pub fn trashed_tag_digest(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<Option<String>, MetaError> {
    Ok(meta
        .get_driver_value(&tag_trash_key(index, repo, tag))?
        .and_then(|raw| serde_json::from_slice::<TagTrash>(&raw).ok())
        .map(|trash| trash.digest))
}

/// # Errors
/// Returns a store error if a scan or read fails.
pub fn list_trashed_tags(meta: &MetaStore, index: &str, repo: &str) -> Result<Vec<String>, MetaError> {
    let prefix = tag_trash_prefix(index, repo);
    let mut tags = Vec::new();
    for key in meta.driver_prefix_keys(&prefix)? {
        if let Some(tag) = key.strip_prefix(prefix.as_str())
            && get_tag(meta, index, repo, tag)?.is_none()
        {
            tags.push(tag.to_owned());
        }
    }
    Ok(tags)
}

/// Every soft-deleted manifest and tag under `index`, as neutral trash records for the inspection
/// view. A trashed tag is one record; a trashed untagged digest is another. A digest deletion that
/// captured tags is represented by those tag records, so it is never listed twice. `retained` reports
/// whether the manifest content a restore needs is still stored, read once per record without walking
/// blob references.
///
/// # Errors
/// Returns a store error if a scan or read fails.
pub fn trash_records(meta: &MetaStore, index: &str) -> Result<Vec<TrashRecord>, MetaError> {
    let mut records = Vec::new();
    let tag_prefix = format!("{TAG_TRASH_PREFIX}{index}\u{0}");
    for key in meta.driver_prefix_keys(&tag_prefix)? {
        if let Some((repo, tag)) = key
            .strip_prefix(tag_prefix.as_str())
            .and_then(|rest| rest.split_once('\u{0}'))
            && let Some(raw) = meta.get_driver_value(&key)?
            && let Ok(trash) = serde_json::from_slice::<TagTrash>(&raw)
        {
            let reference = Some(tag.to_owned());
            records.push(trash_record(meta, index, repo, reference, trash.digest, &trash.info)?);
        }
    }
    let manifest_prefix = format!("{MANIFEST_TRASH_PREFIX}{index}\u{0}");
    for key in meta.driver_prefix_keys(&manifest_prefix)? {
        if let Some((repo, digest)) = key
            .strip_prefix(manifest_prefix.as_str())
            .and_then(|rest| rest.split_once('\u{0}'))
            && let Some(raw) = meta.get_driver_value(&key)?
            && let Ok(trash) = serde_json::from_slice::<ManifestTrash>(&raw)
            && trash.tags.is_empty()
        {
            records.push(trash_record(meta, index, repo, None, digest.to_owned(), &trash.info)?);
        }
    }
    Ok(records)
}

fn trash_record(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    artifact: Option<String>,
    digest: String,
    info: &TrashInfo,
) -> Result<TrashRecord, MetaError> {
    let retained = meta.get_driver_value(&manifest_key(&digest))?.is_some();
    Ok(TrashRecord {
        ecosystem: crate::ECOSYSTEM,
        repository: index.into(),
        resource: repo.into(),
        artifact: artifact.map(Into::into),
        digest: Some(digest),
        reason: info.reason.clone(),
        actor: info.actor.clone(),
        deleted_at_unix: info.deleted_at_unix,
        retained,
    })
}

/// List every repository that has a tag stored under `index`, distinct and sorted. The tag key is
/// `oci\0t\0{index}\0{repo}\0{tag}`, so the repository is the segment between the index and the tag.
///
/// # Errors
/// Returns a store error if the scan fails.
pub fn list_repositories(meta: &MetaStore, index: &str) -> Result<Vec<String>, MetaError> {
    let prefix = format!("{TAG_PREFIX}{index}\u{0}");
    let mut repos = BTreeSet::new();
    for key in meta.driver_prefix_keys(&prefix)? {
        if let Some((repo, _tag)) = key
            .strip_prefix(prefix.as_str())
            .and_then(|rest| rest.rsplit_once('\u{0}'))
        {
            repos.insert(repo.to_owned());
        }
    }
    Ok(repos.into_iter().collect())
}

/// # Errors
/// Returns a store error if the scan fails.
pub fn list_tags(meta: &MetaStore, index: &str, repo: &str) -> Result<Vec<String>, MetaError> {
    let prefix = tag_prefix(index, repo);
    Ok(meta
        .driver_prefix_keys(&prefix)?
        .iter()
        .filter_map(|key| key.strip_prefix(prefix.as_str()).map(str::to_owned))
        .collect())
}

/// # Errors
/// Returns a store error if the scan fails.
pub fn list_tag_targets(meta: &MetaStore, index: &str, repo: &str) -> Result<Vec<(String, String)>, MetaError> {
    let prefix = tag_prefix(index, repo);
    let entries = meta.read_driver_txn(|txn| txn.prefix(&prefix))?;
    let mut targets = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        if let (Some(tag), Ok(digest)) = (key.strip_prefix(prefix.as_str()), std::str::from_utf8(&value)) {
            targets.push((tag.to_owned(), digest.to_owned()));
        }
    }
    Ok(targets)
}

/// # Errors
/// Returns a store error if the write fails.
pub fn delete_tag(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<bool, MetaError> {
    meta.commit_driver_txn(|txn| {
        let removed = txn.remove(&tag_key(index, repo, tag))?;
        txn.remove(&tag_freshness_key(index, repo, tag))?;
        Ok((removed, Vec::new()))
    })
}

/// Move a live tag into repository trash without touching its manifest or blobs.
///
/// # Errors
/// Returns a store error if the transition fails.
pub fn trash_tag(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    tag: &str,
    info: &TrashInfo,
    journal: crate::outbox::Outbox,
    webhook: impl FnOnce(&str) -> Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<Option<String>, MetaError> {
    meta.commit_driver_txn(|txn| {
        let key = tag_key(index, repo, tag);
        let Some(raw) = txn.get(&key)? else {
            return Ok((None, Vec::new()));
        };
        let digest = String::from_utf8_lossy(&raw).into_owned();
        if let Some(webhook) = webhook(&digest) {
            txn.enqueue_webhook_event(webhook);
        }
        let trash = serde_json::to_vec(&TagTrash {
            digest: digest.clone(),
            info: info.clone(),
        })?;
        txn.put(&tag_trash_key(index, repo, tag), &trash)?;
        txn.remove(&key)?;
        txn.remove(&tag_freshness_key(index, repo, tag))?;
        let entries = outbox::record(journal, || OciMutation::TrashTag {
            index: index.to_owned(),
            repo: repo.to_owned(),
            tag: tag.to_owned(),
            digest: digest.clone(),
        });
        Ok((Some(digest), entries))
    })
}

/// # Errors
/// Returns a store error if the transition fails.
pub fn trash_manifest(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    digest: &str,
    info: &TrashInfo,
    journal: crate::outbox::Outbox,
    webhook: Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<Option<usize>, MetaError> {
    meta.commit_driver_txn(|txn| {
        if txn.get(&manifest_trash_key(index, repo, digest))?.is_some()
            || txn.get(&membership_key(index, repo, digest))?.is_none()
            || txn.get(&manifest_key(digest))?.is_none()
        {
            return Ok((None, Vec::new()));
        }
        if let Some(webhook) = webhook {
            txn.enqueue_webhook_event(webhook);
        }
        let tags_prefix = tag_prefix(index, repo);
        let mut tags = Vec::new();
        for (key, target) in txn.prefix(&tags_prefix)? {
            if target != digest.as_bytes() {
                continue;
            }
            let tag = key[tags_prefix.len()..].to_owned();
            let trash = serde_json::to_vec(&TagTrash {
                digest: digest.to_owned(),
                info: info.clone(),
            })?;
            txn.put(&tag_trash_key(index, repo, &tag), &trash)?;
            txn.remove(&key)?;
            txn.remove(&tag_freshness_key(index, repo, &tag))?;
            tags.push(tag);
        }
        let trash = serde_json::to_vec(&ManifestTrash {
            info: info.clone(),
            tags: tags.clone(),
        })?;
        txn.put(&manifest_trash_key(index, repo, digest), &trash)?;
        let count = tags.len();
        let entries = outbox::record(journal, || OciMutation::TrashManifest {
            index: index.to_owned(),
            repo: repo.to_owned(),
            digest: digest.to_owned(),
            tags,
        });
        Ok((Some(count), entries))
    })
}

/// Restore one retained tag. Publishing a tag removes its trash record in the same transaction, so
/// finding the record guarantees that its live slot is free.
///
/// # Errors
/// Returns a store error if the transition fails.
pub fn restore_tag(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    tag: &str,
    journal: crate::outbox::Outbox,
    webhook: impl FnOnce(&str) -> Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<RestoreTagOutcome, MetaError> {
    meta.commit_driver_txn(|txn| {
        let trash_key = tag_trash_key(index, repo, tag);
        let Some(raw) = txn.get(&trash_key)? else {
            return Ok((RestoreTagOutcome::Missing, Vec::new()));
        };
        let trashed = serde_json::from_slice::<TagTrash>(&raw)?;
        if let Some(webhook) = webhook(&trashed.digest) {
            txn.enqueue_webhook_event(webhook);
        }
        txn.put(&tag_key(index, repo, tag), trashed.digest.as_bytes())?;
        txn.remove(&trash_key)?;
        release_trashed_tag(txn, index, repo, &trashed.digest, tag)?;
        let entries = outbox::record(journal, || OciMutation::RestoreTag {
            index: index.to_owned(),
            repo: repo.to_owned(),
            tag: tag.to_owned(),
            digest: trashed.digest.clone(),
        });
        Ok((RestoreTagOutcome::Restored { digest: trashed.digest }, entries))
    })
}

/// Drop a restored tag from its digest's manifest-trash record. A digest deletion saves every tag it
/// captured under one shared record; the record has to outlive each single-tag restore so the tags
/// still trashed keep their parent context, so only the last captured tag's restore clears it. A tag
/// the record never captured leaves it untouched, so an independent untagged deletion of the same
/// digest survives.
fn release_trashed_tag(txn: &mut DriverTxn, index: &str, repo: &str, digest: &str, tag: &str) -> Result<(), MetaError> {
    let key = manifest_trash_key(index, repo, digest);
    let Some(raw) = txn.get(&key)? else {
        return Ok(());
    };
    let mut trashed = serde_json::from_slice::<ManifestTrash>(&raw)?;
    let before = trashed.tags.len();
    trashed.tags.retain(|captured| captured != tag);
    if trashed.tags.len() == before {
        return Ok(());
    }
    if trashed.tags.is_empty() {
        txn.remove(&key)?;
    } else {
        txn.put(&key, &serde_json::to_vec(&trashed)?)?;
    }
    Ok(())
}

/// # Errors
/// Returns a store error if the transition fails.
pub fn restore_manifest(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    digest: &str,
    journal: crate::outbox::Outbox,
    webhook: Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<RestoreManifestOutcome, MetaError> {
    meta.commit_driver_txn(|txn| {
        let trash_key = manifest_trash_key(index, repo, digest);
        let Some(raw) = txn.get(&trash_key)? else {
            return Ok((RestoreManifestOutcome::Missing, Vec::new()));
        };
        if let Some(webhook) = webhook {
            txn.enqueue_webhook_event(webhook);
        }
        let trashed = serde_json::from_slice::<ManifestTrash>(&raw)?;
        let mut restored = Vec::new();
        let mut conflicts = Vec::new();
        for tag in trashed.tags {
            if txn.get(&tag_key(index, repo, &tag))?.is_some() {
                conflicts.push(tag);
            } else {
                txn.put(&tag_key(index, repo, &tag), digest.as_bytes())?;
                txn.remove(&tag_trash_key(index, repo, &tag))?;
                restored.push(tag);
            }
        }
        txn.remove(&trash_key)?;
        let entries = outbox::record(journal, || OciMutation::RestoreManifest {
            index: index.to_owned(),
            repo: repo.to_owned(),
            digest: digest.to_owned(),
            tags: restored.clone(),
        });
        Ok((RestoreManifestOutcome::Restored { restored, conflicts }, entries))
    })
}

/// The driver-KV key one upstream tag-list page lives under. The query is part of the key: `?n=` and
/// `?last=` select different pages, and one must never answer for another.
fn tag_page_key(index: &str, repo: &str, query: &str) -> String {
    format!("oci\u{0}tp\u{0}{index}\u{0}{repo}\u{0}{query}")
}

/// # Errors
/// Returns a store error if the write fails.
pub fn set_tag_page(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    query: &str,
    at: i64,
    link: Option<&str>,
    body: &[u8],
) -> Result<(), MetaError> {
    let link = link.unwrap_or_default().as_bytes();
    let length = u32::try_from(link.len()).unwrap_or(u32::MAX);
    let mut value = at.to_be_bytes().to_vec();
    value.extend_from_slice(&length.to_be_bytes());
    value.extend_from_slice(link);
    value.extend_from_slice(body);
    meta.put_driver_value(&tag_page_key(index, repo, query), &value)
}

/// A stored tag-list page: when it was fetched, the `Link` to the next page, and the body.
pub type TagPage = (i64, Option<String>, Vec<u8>);

/// # Errors
/// Returns a store error if the read fails.
pub fn tag_page(meta: &MetaStore, index: &str, repo: &str, query: &str) -> Result<Option<TagPage>, MetaError> {
    let Some(raw) = meta.get_driver_value(&tag_page_key(index, repo, query))? else {
        return Ok(None);
    };
    let Some((at, rest)) = raw.split_first_chunk::<8>() else {
        return Ok(None);
    };
    let Some((length, rest)) = rest.split_first_chunk::<4>() else {
        return Ok(None);
    };
    let length = u32::from_be_bytes(*length) as usize;
    if rest.len() < length {
        return Ok(None);
    }
    let (link, body) = rest.split_at(length);
    let link = (!link.is_empty()).then(|| String::from_utf8_lossy(link).into_owned());
    Ok(Some((i64::from_be_bytes(*at), link, body.to_vec())))
}

fn referrer_page_key(index: &str, repo: &str, subject: &str) -> String {
    format!("{REFERRER_PAGE_PREFIX}{index}\u{0}{repo}\u{0}{subject}")
}

/// Cache one validated upstream referrers result. Failed revalidation never reaches this write, so it
/// cannot replace a known result with an inferred empty list.
///
/// # Errors
/// Returns a store error if serialization or the write fails.
pub fn set_referrer_page(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    subject: &str,
    at: i64,
    manifests: &[serde_json::Value],
) -> Result<(), MetaError> {
    let mut value = at.to_be_bytes().to_vec();
    value.extend_from_slice(&serde_json::to_vec(manifests)?);
    meta.put_driver_value(&referrer_page_key(index, repo, subject), &value)
}

/// The last validated upstream referrers result and its fetch time.
///
/// # Errors
/// Returns a store error if the read or decode fails.
pub fn referrer_page(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    subject: &str,
) -> Result<Option<(i64, Vec<serde_json::Value>)>, MetaError> {
    let Some(raw) = meta.get_driver_value(&referrer_page_key(index, repo, subject))? else {
        return Ok(None);
    };
    let Some((at, manifests)) = raw.split_first_chunk::<8>() else {
        return Ok(None);
    };
    Ok(Some((i64::from_be_bytes(*at), serde_json::from_slice(manifests)?)))
}

fn tag_freshness_key(index: &str, repo: &str, tag: &str) -> String {
    format!("oci\u{0}tf\u{0}{index}\u{0}{repo}\u{0}{tag}")
}

/// Record that a proxy revalidated `tag` to `digest` at `at` (unix seconds), so a repeat pull within
/// the freshness window serves the cached manifest instead of counting another upstream fetch.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn set_tag_freshness(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    tag: &str,
    digest: &str,
    at: i64,
) -> Result<(), MetaError> {
    let mut value = at.to_be_bytes().to_vec();
    value.extend_from_slice(digest.as_bytes());
    meta.put_driver_value(&tag_freshness_key(index, repo, tag), &value)
}

/// The `(fetched_at, digest)` a proxy last recorded for `tag`, or `None` if it never fetched it.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn tag_freshness(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<Option<(i64, String)>, MetaError> {
    let Some(raw) = meta.get_driver_value(&tag_freshness_key(index, repo, tag))? else {
        return Ok(None);
    };
    let Some((at, digest)) = raw.split_first_chunk::<8>() else {
        return Ok(None);
    };
    Ok(String::from_utf8(digest.to_vec())
        .ok()
        .map(|digest| (i64::from_be_bytes(*at), digest)))
}

/// Record that the manifest `referrer` declares `subject` as its subject in `repo`, storing its
/// descriptor for the referrers API. Keyed by the subject so a referrers query is a prefix scan.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_referrer(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    subject: &str,
    referrer: &str,
    descriptor: &[u8],
) -> Result<(), MetaError> {
    meta.put_driver_value(
        &format!("{}{referrer}", referrer_prefix(index, repo, subject)),
        descriptor,
    )
}

/// # Errors
/// Returns a store error if the scan fails.
pub fn list_referrers(meta: &MetaStore, index: &str, repo: &str, subject: &str) -> Result<Vec<Vec<u8>>, MetaError> {
    let prefix = referrer_prefix(index, repo, subject);
    let mut descriptors = Vec::new();
    for key in meta.driver_prefix_keys(&prefix)? {
        if let Some(value) = meta.get_driver_value(&key)? {
            descriptors.push(value);
        }
    }
    Ok(descriptors)
}

fn referrer_prefix(index: &str, repo: &str, subject: &str) -> String {
    format!("{REFERRER_PREFIX}{index}\u{0}{repo}\u{0}{subject}\u{0}")
}

#[cfg(test)]
#[path = "../tests/unit/store/tests.rs"]
mod tests;
