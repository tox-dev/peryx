//! How the OCI driver lays its data into the neutral stores.
//!
//! Blobs and manifests are content-addressed, so they share the global
//! [`BlobStorage`](peryx_storage::blob::BlobStorage)/manifest
//! namespace and dedupe across proxies. Tags are mutable per proxy, so a tag key carries the index
//! route and the upstream repository to keep two registries' identically-named repos apart. The
//! [`MetaStore`] never interprets these keys; the driver owns the whole layout.

use std::collections::BTreeSet;

pub use peryx_core::TrashInfo;
use peryx_core::TrashRecord;
use peryx_storage::meta::{ArtifactOrigin, ArtifactSource, DriverTxn, MetaError, MetaStore};
use serde::{Deserialize, Serialize};

use crate::outbox::{self, OciMutation};

/// The driver-KV prefix every manifest is keyed under, its digest following.
mod descriptors;
pub use descriptors::{blob_digest, linux_amd64_child, manifest_descriptors, referenced_blob_digests};

const MANIFEST_PREFIX: &str = "oci\u{0}m\u{0}";
const TAG_PREFIX: &str = "oci\u{0}t\u{0}";
const REFERRER_PREFIX: &str = "oci\u{0}r\u{0}";
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
    /// Encode as a `u16` media-type length, the media type, then the body.
    fn encode(&self) -> Vec<u8> {
        let media_type = self.media_type.as_bytes();
        let mut out = Vec::with_capacity(2 + media_type.len() + self.bytes.len());
        out.extend_from_slice(&u16::try_from(media_type.len()).unwrap_or(u16::MAX).to_be_bytes());
        out.extend_from_slice(media_type);
        out.extend_from_slice(&self.bytes);
        out
    }

    /// Decode the length-prefixed form, or `None` if the bytes are truncated.
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

/// The driver-KV key a manifest is stored under: its digest, globally, since the bytes are the same
/// wherever the manifest came from.
fn manifest_key(digest: &str) -> String {
    format!("{MANIFEST_PREFIX}{digest}")
}

/// The driver-KV key a tag resolves under, scoped to the proxy and the upstream repository.
fn tag_key(index: &str, repo: &str, tag: &str) -> String {
    format!("{TAG_PREFIX}{index}\u{0}{repo}\u{0}{tag}")
}

/// The prefix that enumerates every tag of one repository under one proxy.
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

/// Store a manifest under its digest, without recording membership — a test seed for the by-digest
/// read and delete paths that expects a manifest present in the global pool but served by no
/// repository.
///
/// # Errors
/// Returns a store error if the write fails.
#[cfg(test)]
pub fn put_manifest(meta: &MetaStore, digest: &str, manifest: &Manifest) -> Result<(), MetaError> {
    meta.put_driver_value(&manifest_key(digest), &manifest.encode())
}

/// Store a manifest and record it as one `(index, repo)` serves: its own digest, and — for an image
/// index or manifest list — each child it names. A by-digest read authorizes against this per-repository
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
pub fn record_manifest_txn(
    txn: &mut DriverTxn,
    index: &str,
    repo: &str,
    digest: &str,
    manifest: &Manifest,
) -> Result<(), MetaError> {
    txn.put(&manifest_key(digest), &manifest.encode())?;
    txn.put(&membership_key(index, repo, digest), &[])?;
    let (children, blobs) = manifest_descriptors(&manifest.bytes);
    for child in children {
        txn.put(&membership_key(index, repo, &child), &[])?;
    }
    for blob in blobs {
        txn.put(&blob_membership_key(index, repo, &blob), &[])?;
    }
    Ok(())
}

/// The driver-KV key marking that `(index, repo)` serves `digest`. The value is empty: the key's
/// presence is the authorization a by-digest read checks.
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
    meta.record_artifact_placement(digest, origin.artifact_source(), present)?;
    Ok(())
}

/// Fetch a manifest by digest.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn get_manifest(meta: &MetaStore, digest: &str) -> Result<Option<Manifest>, MetaError> {
    Ok(meta
        .get_driver_value(&manifest_key(digest))?
        .and_then(|raw| Manifest::decode(&raw)))
}

/// Whether `(index, repo)` records `digest` as one it serves — the authorization for a by-digest read.
///
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

/// Returns `true` when the searchable tag set grows; retargeting an existing tag returns `false`.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_tag(meta: &MetaStore, index: &str, repo: &str, tag: &str, digest: &str) -> Result<bool, MetaError> {
    meta.commit_driver_txn(|txn| Ok((put_tag_txn(txn, index, repo, tag, digest)?, Vec::new())))
}

/// Point `tag` at `digest` inside an open transaction, reporting whether the searchable tag set grew,
/// so a caller can publish it atomically with a quota-reservation commit.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn put_tag_txn(txn: &mut DriverTxn, index: &str, repo: &str, tag: &str, digest: &str) -> Result<bool, MetaError> {
    txn.upsert(&tag_key(index, repo, tag), digest.as_bytes())
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
) -> Result<bool, MetaError> {
    record_manifest_txn(txn, index, repo, digest, manifest)?;
    txn.remove(&manifest_trash_key(index, repo, digest))?;
    let Some(tag) = tag else {
        return Ok(false);
    };
    txn.remove(&tag_trash_key(index, repo, tag))?;
    put_tag_txn(txn, index, repo, tag, digest)
}

/// Resolve a tag to its cached manifest digest.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn get_tag(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<Option<String>, MetaError> {
    Ok(meta
        .get_driver_value(&tag_key(index, repo, tag))?
        .and_then(|raw| String::from_utf8(raw).ok()))
}

/// Whether a repository has hidden `digest` in its manifest trash.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn manifest_is_trashed(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<bool, MetaError> {
    Ok(meta
        .get_driver_value(&manifest_trash_key(index, repo, digest))?
        .is_some())
}

/// Whether a repository has hidden `tag` and no live value currently supersedes that deletion.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn tag_is_trashed(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<bool, MetaError> {
    Ok(
        get_tag(meta, index, repo, tag)?.is_none()
            && meta.get_driver_value(&tag_trash_key(index, repo, tag))?.is_some(),
    )
}

/// Resolve a tag's retained trash record.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn trashed_tag_digest(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<Option<String>, MetaError> {
    Ok(meta
        .get_driver_value(&tag_trash_key(index, repo, tag))?
        .and_then(|raw| serde_json::from_slice::<TagTrash>(&raw).ok())
        .map(|trash| trash.digest))
}

/// List hidden tag names for one repository, in key order.
///
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
    reference: Option<String>,
    digest: String,
    info: &TrashInfo,
) -> Result<TrashRecord, MetaError> {
    let retained = meta.get_driver_value(&manifest_key(&digest))?.is_some();
    Ok(TrashRecord {
        ecosystem: crate::ECOSYSTEM,
        repository: index.to_owned(),
        name: repo.to_owned(),
        reference,
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

/// List every cached tag of a repository under a proxy, in key order.
///
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

/// List each cached tag and its manifest digest under one repository snapshot.
///
/// # Errors
/// Returns a store error if the scan fails.
pub fn list_tag_targets(meta: &MetaStore, index: &str, repo: &str) -> Result<Vec<(String, String)>, MetaError> {
    let prefix = tag_prefix(index, repo);
    let mut targets = Vec::new();
    meta.visit_driver_prefix(&prefix, |key, value| {
        if let (Some(tag), Ok(digest)) = (key.strip_prefix(prefix.as_str()), std::str::from_utf8(value)) {
            targets.push((tag.to_owned(), digest.to_owned()));
        }
    })?;
    Ok(targets)
}

/// Remove a manifest by digest, reporting whether it was present.
///
/// # Errors
/// Returns a store error if the write fails.
#[cfg(test)]
pub fn delete_manifest(meta: &MetaStore, digest: &str) -> Result<bool, MetaError> {
    meta.delete_driver_value(&manifest_key(digest))
}

/// Remove a tag, reporting whether it was present. Its proxy freshness record goes with it.
///
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
    journal: bool,
) -> Result<Option<String>, MetaError> {
    meta.commit_driver_txn(|txn| {
        let key = tag_key(index, repo, tag);
        let Some(raw) = txn.get(&key)? else {
            return Ok((None, Vec::new()));
        };
        let digest = String::from_utf8_lossy(&raw).into_owned();
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

/// Move a repository's manifest digest and every tag currently pointing to it into trash atomically.
/// The content, memberships, descriptors, and blob references remain intact for restore.
///
/// # Errors
/// Returns a store error if the transition fails.
pub fn trash_manifest(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    digest: &str,
    info: &TrashInfo,
    journal: bool,
) -> Result<Option<usize>, MetaError> {
    meta.commit_driver_txn(|txn| {
        if txn.get(&manifest_trash_key(index, repo, digest))?.is_some()
            || txn.get(&membership_key(index, repo, digest))?.is_none()
            || txn.get(&manifest_key(digest))?.is_none()
        {
            return Ok((None, Vec::new()));
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
    journal: bool,
) -> Result<RestoreTagOutcome, MetaError> {
    meta.commit_driver_txn(|txn| {
        let trash_key = tag_trash_key(index, repo, tag);
        let Some(raw) = txn.get(&trash_key)? else {
            return Ok((RestoreTagOutcome::Missing, Vec::new()));
        };
        let trashed = serde_json::from_slice::<TagTrash>(&raw)?;
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

/// Restore one digest and every captured tag whose live slot remains free. Tags republished with
/// another digest remain live and are reported as conflicts.
///
/// # Errors
/// Returns a store error if the transition fails.
pub fn restore_manifest(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    digest: &str,
    journal: bool,
) -> Result<RestoreManifestOutcome, MetaError> {
    meta.commit_driver_txn(|txn| {
        let trash_key = manifest_trash_key(index, repo, digest);
        let Some(raw) = txn.get(&trash_key)? else {
            return Ok((RestoreManifestOutcome::Missing, Vec::new()));
        };
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

/// Record an upstream tag-list page: when it was fetched, the `Link` header that names the next one,
/// and the body verbatim.
///
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

/// The stored tag-list page for `query`, or `None` if none was ever fetched.
///
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

/// The driver-KV key a proxy tag's last-fetch record lives under.
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

/// The descriptors of every manifest that declares `subject` as its subject in `repo`, in digest
/// order.
///
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

/// The driver-KV key prefix referrer descriptors live under, scoped to the index, repo, and subject.
fn referrer_prefix(index: &str, repo: &str, subject: &str) -> String {
    format!("{REFERRER_PREFIX}{index}\u{0}{repo}\u{0}{subject}\u{0}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    #[test]
    fn test_tag_page_round_trips_with_and_without_a_link() {
        let (_dir, meta) = store();
        set_tag_page(&meta, "hub", "library/nginx", "", 42, Some("</v2/x?n=1>"), b"{}").unwrap();
        assert_eq!(
            tag_page(&meta, "hub", "library/nginx", "").unwrap(),
            Some((42, Some("</v2/x?n=1>".to_owned()), b"{}".to_vec()))
        );

        set_tag_page(&meta, "hub", "library/nginx", "n=1", 7, None, b"[]").unwrap();
        assert_eq!(
            tag_page(&meta, "hub", "library/nginx", "n=1").unwrap(),
            Some((7, None, b"[]".to_vec()))
        );
    }

    #[test]
    fn test_a_truncated_tag_page_record_reads_as_absent() {
        let (_dir, meta) = store();
        // Anything shorter than the header, or claiming a link longer than the bytes that follow, is
        // not a page. Answering with a fragment of one would be worse than fetching it again.
        for raw in [
            vec![0u8; 4],  // no timestamp
            vec![0u8; 10], // timestamp, no link length
            [&0i64.to_be_bytes()[..], &99u32.to_be_bytes()[..], b"x"].concat(),
        ] {
            meta.put_driver_value(&tag_page_key("hub", "repo", ""), &raw).unwrap();
            assert_eq!(tag_page(&meta, "hub", "repo", "").unwrap(), None, "{raw:?}");
        }
    }

    #[test]
    fn test_manifest_round_trips_through_the_store() {
        let (_dir, meta) = store();
        let manifest = Manifest {
            media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
            bytes: b"{\"schemaVersion\":2}".to_vec(),
        };
        put_manifest(&meta, "sha256:abc", &manifest).unwrap();
        assert_eq!(get_manifest(&meta, "sha256:abc").unwrap(), Some(manifest));
        assert_eq!(get_manifest(&meta, "sha256:missing").unwrap(), None);
    }

    #[test]
    fn test_origin_maps_to_the_neutral_source() {
        assert_eq!(OciArtifactOrigin::Pushed.artifact_source(), ArtifactSource::Hosted);
        assert_eq!(OciArtifactOrigin::Mirrored.artifact_source(), ArtifactSource::Proxy);
    }

    #[test]
    fn test_content_placement_records_local_and_reprojects_on_eviction() {
        use peryx_storage::meta::{ByteAvailability, PlacementEvent};

        let (_dir, meta) = store();
        record_content_placement(&meta, "sha256:abc", OciArtifactOrigin::Pushed, true).unwrap();
        assert_eq!(
            meta.get_artifact_placement("sha256:abc").unwrap().unwrap().availability,
            ByteAvailability::Local
        );
        record_content_placement(&meta, "sha256:def", OciArtifactOrigin::Mirrored, true).unwrap();
        let evicted = meta
            .apply_placement_event("sha256:def", PlacementEvent::BytesRemoved)
            .unwrap()
            .unwrap();
        assert_eq!(evicted.availability, ByteAvailability::RemoteOnly);
    }

    #[test]
    fn test_decode_rejects_truncated_manifest() {
        assert_eq!(Manifest::decode(&[0x00]), None);
        assert_eq!(Manifest::decode(&[0x00, 0x05, b'a']), None);
    }

    #[test]
    fn test_tag_freshness_round_trips_and_rejects_corrupt_records() {
        let (_dir, meta) = store();
        assert_eq!(tag_freshness(&meta, "hub", "repo", "latest").unwrap(), None);
        set_tag_freshness(&meta, "hub", "repo", "latest", "sha256:abc", 1234).unwrap();
        assert_eq!(
            tag_freshness(&meta, "hub", "repo", "latest").unwrap(),
            Some((1234, "sha256:abc".to_owned()))
        );
        // A record too short for the timestamp prefix, or one with a non-utf8 digest, reads as absent.
        meta.put_driver_value(&tag_freshness_key("hub", "repo", "short"), &[0x00])
            .unwrap();
        assert_eq!(tag_freshness(&meta, "hub", "repo", "short").unwrap(), None);
        let mut corrupt = 5i64.to_be_bytes().to_vec();
        corrupt.push(0xff);
        meta.put_driver_value(&tag_freshness_key("hub", "repo", "badutf"), &corrupt)
            .unwrap();
        assert_eq!(tag_freshness(&meta, "hub", "repo", "badutf").unwrap(), None);
        // Deleting the tag removes its freshness record too.
        put_tag(&meta, "hub", "repo", "latest", "sha256:abc").unwrap();
        delete_tag(&meta, "hub", "repo", "latest").unwrap();
        assert_eq!(tag_freshness(&meta, "hub", "repo", "latest").unwrap(), None);
    }

    #[test]
    fn test_tags_scope_to_index_and_repo_and_sort() {
        let (_dir, meta) = store();
        put_tag(&meta, "hub", "library/nginx", "latest", "sha256:1").unwrap();
        put_tag(&meta, "hub", "library/nginx", "1.25", "sha256:2").unwrap();
        put_tag(&meta, "hub", "library/other", "latest", "sha256:3").unwrap();
        put_tag(&meta, "gitlab", "library/nginx", "edge", "sha256:9").unwrap();
        assert_eq!(
            get_tag(&meta, "hub", "library/nginx", "latest").unwrap(),
            Some("sha256:1".to_owned())
        );
        assert_eq!(get_tag(&meta, "hub", "library/nginx", "absent").unwrap(), None);
        assert_eq!(
            list_tags(&meta, "hub", "library/nginx").unwrap(),
            vec!["1.25", "latest"]
        );
        assert_eq!(
            list_tag_targets(&meta, "hub", "library/nginx").unwrap(),
            vec![
                ("1.25".to_owned(), "sha256:2".to_owned()),
                ("latest".to_owned(), "sha256:1".to_owned()),
            ]
        );
    }

    #[test]
    fn test_put_tag_reports_insert_and_repoints() {
        let (_dir, meta) = store();

        assert_eq!(
            (
                put_tag(&meta, "hub", "library/nginx", "latest", "sha256:1").unwrap(),
                put_tag(&meta, "hub", "library/nginx", "latest", "sha256:2").unwrap(),
                get_tag(&meta, "hub", "library/nginx", "latest").unwrap()
            ),
            (true, false, Some("sha256:2".to_owned()))
        );
    }

    #[test]
    fn test_referrers_scope_to_index_repo_and_subject() {
        let (_dir, meta) = store();
        put_referrer(
            &meta,
            "store",
            "app",
            "sha256:subj",
            "sha256:ref1",
            b"{\"digest\":\"sha256:ref1\"}",
        )
        .unwrap();
        put_referrer(
            &meta,
            "store",
            "app",
            "sha256:subj",
            "sha256:ref2",
            b"{\"digest\":\"sha256:ref2\"}",
        )
        .unwrap();
        put_referrer(
            &meta,
            "store",
            "other",
            "sha256:subj",
            "sha256:ref3",
            b"{\"digest\":\"sha256:ref3\"}",
        )
        .unwrap();
        put_referrer(&meta, "store", "app", "sha256:elsewhere", "sha256:ref4", b"{}").unwrap();

        let referrers = list_referrers(&meta, "store", "app", "sha256:subj").unwrap();
        assert_eq!(referrers.len(), 2);
        assert!(referrers.iter().any(|value| value == b"{\"digest\":\"sha256:ref1\"}"));
        assert!(referrers.iter().any(|value| value == b"{\"digest\":\"sha256:ref2\"}"));
        assert!(list_referrers(&meta, "store", "app", "sha256:none").unwrap().is_empty());
    }

    fn index_of(child: &str) -> Manifest {
        Manifest {
            media_type: "application/vnd.oci.image.index.v1+json".to_owned(),
            bytes: format!(r#"{{"manifests":[{{"digest":"{child}"}}]}}"#).into_bytes(),
        }
    }

    #[test]
    fn test_record_manifest_marks_the_manifest_and_its_index_children() {
        let (_dir, meta) = store();
        let child = format!("sha256:{}", "c".repeat(64));
        record_manifest(&meta, "store", "app", "sha256:idx", &index_of(&child)).unwrap();
        // The index and the child it names are both ones this repo serves; another repo and an
        // unrecorded digest are not.
        assert!(manifest_is_member(&meta, "store", "app", "sha256:idx").unwrap());
        assert!(manifest_is_member(&meta, "store", "app", &child).unwrap());
        assert!(!manifest_is_member(&meta, "store", "other", "sha256:idx").unwrap());
        assert!(!manifest_is_member(&meta, "store", "app", "sha256:absent").unwrap());
    }

    #[test]
    fn test_blob_membership_is_repository_scoped() {
        let (_dir, meta, digest) = blob_member();

        assert_eq!(
            (
                blob_is_member(&meta, "store", "app", &digest).unwrap(),
                blob_is_member(&meta, "store", "other", &digest).unwrap(),
            ),
            (true, false)
        );
    }

    #[test]
    fn test_record_manifest_marks_its_blob_descriptors() {
        let (_dir, meta) = store();
        let config = format!("sha256:{}", "a".repeat(64));
        let layer = format!("sha256:{}", "b".repeat(64));
        let manifest = Manifest {
            media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
            bytes: format!(r#"{{"config":{{"digest":"{config}"}},"layers":[{{"digest":"{layer}"}}]}}"#).into_bytes(),
        };

        record_manifest(&meta, "store", "app", "sha256:manifest", &manifest).unwrap();

        assert_eq!(
            (
                blob_is_member(&meta, "store", "app", &config).unwrap(),
                blob_is_member(&meta, "store", "app", &layer).unwrap(),
                blob_is_member(&meta, "store", "other", &config).unwrap(),
            ),
            (true, true, false)
        );
    }

    fn image(bytes: &str) -> Manifest {
        Manifest {
            media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
            bytes: bytes.as_bytes().to_vec(),
        }
    }

    fn info() -> TrashInfo {
        TrashInfo {
            deleted_at_unix: 100,
            actor: Some("alice".to_owned()),
            reason: Some("bad build".to_owned()),
        }
    }

    #[test]
    fn test_trash_records_lists_a_trashed_tag_with_provenance_and_retention() {
        let (_dir, meta) = store();
        record_manifest(&meta, "hub", "library/nginx", "sha256:a", &image("{}")).unwrap();
        put_tag(&meta, "hub", "library/nginx", "latest", "sha256:a").unwrap();
        trash_tag(&meta, "hub", "library/nginx", "latest", &info(), false).unwrap();

        let records = trash_records(&meta, "hub").unwrap();

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.ecosystem, crate::ECOSYSTEM);
        assert_eq!(record.repository, "hub");
        assert_eq!(record.name, "library/nginx");
        assert_eq!(record.reference.as_deref(), Some("latest"));
        assert_eq!(record.digest.as_deref(), Some("sha256:a"));
        assert_eq!(record.reason.as_deref(), Some("bad build"));
        assert_eq!(record.actor.as_deref(), Some("alice"));
        assert_eq!(record.deleted_at_unix, 100);
        assert!(record.retained, "the manifest content is still stored");
    }

    #[test]
    fn test_trash_records_reports_an_untagged_digest_deletion_once() {
        let (_dir, meta) = store();
        record_manifest(&meta, "hub", "library/nginx", "sha256:a", &image("{}")).unwrap();
        trash_manifest(&meta, "hub", "library/nginx", "sha256:a", &info(), false).unwrap();

        let records = trash_records(&meta, "hub").unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].reference, None, "an untagged digest carries no tag");
        assert_eq!(records[0].digest.as_deref(), Some("sha256:a"));
    }

    #[test]
    fn test_trash_records_does_not_double_count_a_tagged_manifest_deletion() {
        let (_dir, meta) = store();
        record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
        put_tag(&meta, "hub", "app", "1.0", "sha256:a").unwrap();
        put_tag(&meta, "hub", "app", "latest", "sha256:a").unwrap();
        trash_manifest(&meta, "hub", "app", "sha256:a", &info(), false).unwrap();

        let mut references: Vec<Option<String>> = trash_records(&meta, "hub")
            .unwrap()
            .into_iter()
            .map(|record| record.reference)
            .collect();
        references.sort();

        assert_eq!(
            references,
            vec![Some("1.0".to_owned()), Some("latest".to_owned())],
            "the two captured tags are the only records, not a third digest row"
        );
    }

    #[test]
    fn test_trash_records_marks_purged_content_as_not_retained() {
        let (_dir, meta) = store();
        record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
        put_tag(&meta, "hub", "app", "latest", "sha256:a").unwrap();
        trash_tag(&meta, "hub", "app", "latest", &info(), false).unwrap();
        delete_manifest(&meta, "sha256:a").unwrap();

        let records = trash_records(&meta, "hub").unwrap();

        assert_eq!(records.len(), 1);
        assert!(!records[0].retained, "purged content is not restorable");
    }

    #[test]
    fn test_trash_records_scope_to_one_index_and_skip_corrupt_rows() {
        let (_dir, meta) = store();
        record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
        put_tag(&meta, "hub", "app", "latest", "sha256:a").unwrap();
        trash_tag(&meta, "hub", "app", "latest", &info(), false).unwrap();
        meta.put_driver_value(&tag_trash_key("hub", "app", "corrupt"), b"not json")
            .unwrap();

        assert_eq!(
            trash_records(&meta, "hub").unwrap().len(),
            1,
            "the corrupt row is skipped"
        );
        assert!(
            trash_records(&meta, "other").unwrap().is_empty(),
            "records scope to the index"
        );
    }

    #[test]
    fn test_restore_tag_reports_a_missing_tag() {
        let (_dir, meta) = store();
        assert_eq!(
            restore_tag(&meta, "hub", "app", "absent", false).unwrap(),
            RestoreTagOutcome::Missing
        );
    }

    #[test]
    fn test_restore_tag_without_a_manifest_record_restores_the_tag() {
        let (_dir, meta) = store();
        record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
        put_tag(&meta, "hub", "app", "v1", "sha256:a").unwrap();
        trash_tag(&meta, "hub", "app", "v1", &info(), false).unwrap();

        assert_eq!(
            restore_tag(&meta, "hub", "app", "v1", false).unwrap(),
            RestoreTagOutcome::Restored {
                digest: "sha256:a".to_owned()
            }
        );
        assert_eq!(get_tag(&meta, "hub", "app", "v1").unwrap(), Some("sha256:a".to_owned()));
    }

    #[test]
    fn test_restore_tag_keeps_shared_manifest_trash_until_the_last_tag() {
        let (_dir, meta) = store();
        record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
        put_tag(&meta, "hub", "app", "v1", "sha256:a").unwrap();
        put_tag(&meta, "hub", "app", "v2", "sha256:a").unwrap();
        trash_manifest(&meta, "hub", "app", "sha256:a", &info(), false).unwrap();

        assert_eq!(
            restore_tag(&meta, "hub", "app", "v1", false).unwrap(),
            RestoreTagOutcome::Restored {
                digest: "sha256:a".to_owned()
            }
        );
        // v1 is live again, yet the digest stays trashed because v2's restore still needs the record.
        assert_eq!(get_tag(&meta, "hub", "app", "v1").unwrap(), Some("sha256:a".to_owned()));
        assert!(manifest_is_trashed(&meta, "hub", "app", "sha256:a").unwrap());
        assert_eq!(list_trashed_tags(&meta, "hub", "app").unwrap(), vec!["v2"]);

        assert_eq!(
            restore_tag(&meta, "hub", "app", "v2", false).unwrap(),
            RestoreTagOutcome::Restored {
                digest: "sha256:a".to_owned()
            }
        );
        // Restoring the last captured tag clears the shared record.
        assert!(!manifest_is_trashed(&meta, "hub", "app", "sha256:a").unwrap());
        assert!(list_trashed_tags(&meta, "hub", "app").unwrap().is_empty());
    }

    #[test]
    fn test_restore_tag_leaves_an_independent_untagged_deletion() {
        let (_dir, meta) = store();
        record_manifest(&meta, "hub", "app", "sha256:a", &image("{}")).unwrap();
        put_tag(&meta, "hub", "app", "v1", "sha256:a").unwrap();
        // v1 is trashed on its own, then the still-live untagged digest is trashed separately, so the
        // manifest record captures no tags.
        trash_tag(&meta, "hub", "app", "v1", &info(), false).unwrap();
        trash_manifest(&meta, "hub", "app", "sha256:a", &info(), false).unwrap();

        restore_tag(&meta, "hub", "app", "v1", false).unwrap();

        assert_eq!(get_tag(&meta, "hub", "app", "v1").unwrap(), Some("sha256:a".to_owned()));
        assert!(
            manifest_is_trashed(&meta, "hub", "app", "sha256:a").unwrap(),
            "the untagged digest deletion is not this tag's to undo"
        );
    }

    fn blob_member() -> (tempfile::TempDir, MetaStore, String) {
        let (dir, meta) = store();
        let digest = format!("sha256:{}", "a".repeat(64));
        record_blob_membership(&meta, "store", "app", &digest).unwrap();
        (dir, meta, digest)
    }
}
