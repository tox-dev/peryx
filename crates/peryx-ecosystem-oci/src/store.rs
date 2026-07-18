//! How the OCI driver lays its data into the neutral stores.
//!
//! Blobs and manifests are content-addressed, so they share the global
//! [`BlobStorage`](peryx_storage::blob::BlobStorage)/manifest
//! namespace and dedupe across proxies. Tags are mutable per proxy, so a tag key carries the index
//! route and the upstream repository to keep two registries' identically-named repos apart. The
//! [`MetaStore`] never interprets these keys; the driver owns the whole layout.

use std::collections::BTreeSet;

use peryx_storage::meta::{DriverTxn, MetaError, MetaStore};

/// The driver-KV prefix every manifest is keyed under, its digest following.
mod descriptors;
pub use descriptors::{blob_digest, linux_amd64_child, manifest_descriptors, referenced_blob_digests};

const MANIFEST_PREFIX: &str = "oci\u{0}m\u{0}";
const TAG_PREFIX: &str = "oci\u{0}t\u{0}";
const REFERRER_PREFIX: &str = "oci\u{0}r\u{0}";
const MEMBERSHIP_PREFIX: &str = "oci\u{0}mm\u{0}";
const BLOB_MEMBERSHIP_PREFIX: &str = "oci\u{0}bm\u{0}";

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

/// The prefix enumerating every digest `(index, repo)` records as one it serves.
fn membership_prefix(index: &str, repo: &str) -> String {
    format!("{MEMBERSHIP_PREFIX}{index}\u{0}{repo}\u{0}")
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

/// # Errors
/// Returns a store error if the write fails.
pub fn delete_blob_membership(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<bool, MetaError> {
    meta.delete_driver_value(&blob_membership_key(index, repo, digest))
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

/// Resolve a tag to its cached manifest digest.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn get_tag(meta: &MetaStore, index: &str, repo: &str, tag: &str) -> Result<Option<String>, MetaError> {
    Ok(meta
        .get_driver_value(&tag_key(index, repo, tag))?
        .and_then(|raw| String::from_utf8(raw).ok()))
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

/// Remove a manifest by digest, reporting whether it was present.
///
/// # Errors
/// Returns a store error if the write fails.
pub fn delete_manifest(meta: &MetaStore, digest: &str) -> Result<bool, MetaError> {
    meta.delete_driver_value(&manifest_key(digest))
}

/// Drop `(index, repo)`'s record that it serves `digest`, unless the digest is still a child of an
/// image index or manifest list this repository serves — a child stays reachable, and readable by
/// digest, for as long as an index that names it does. Called on a by-digest manifest delete, after its
/// tags and referrer records are unlinked, so a deleted digest cannot later be read by digest under a
/// repository that no longer holds it (which a re-push elsewhere would otherwise leak).
///
/// # Errors
/// Returns a store error if a scan, read, or write fails.
pub fn prune_manifest_membership(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<(), MetaError> {
    if !reachable_as_child(meta, index, repo, digest)? {
        meta.delete_driver_value(&membership_key(index, repo, digest))?;
    }
    Ok(())
}

/// Whether some other manifest `(index, repo)` serves names `digest` among its children.
fn reachable_as_child(meta: &MetaStore, index: &str, repo: &str, digest: &str) -> Result<bool, MetaError> {
    let prefix = membership_prefix(index, repo);
    for key in meta.driver_prefix_keys(&prefix)? {
        let member = &key[prefix.len()..];
        if member != digest
            && let Some(manifest) = get_manifest(meta, member)?
            && manifest_descriptors(&manifest.bytes)
                .0
                .iter()
                .any(|child| child == digest)
        {
            return Ok(true);
        }
    }
    Ok(false)
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

/// Every manifest digest something still points at: a tag target in any index, an image index's
/// child, or a referrer record's own digest or its subject. A digest absent from this set is reachable
/// from nothing, so a delete may unlink it; one present is retained exactly as a referenced blob is.
///
/// A full driver-KV scan, run only on the (rare) manifest-DELETE path, never on a read — the same cost
/// [`referenced_blob_digests`] already accepts.
///
/// # Errors
/// Returns a store error if a scan or read fails.
pub fn referenced_manifest_digests(meta: &MetaStore) -> Result<BTreeSet<String>, MetaError> {
    let mut digests = BTreeSet::new();
    for key in meta.driver_prefix_keys(TAG_PREFIX)? {
        if let Some(target) = meta.get_driver_value(&key)?.and_then(|raw| String::from_utf8(raw).ok()) {
            digests.insert(target);
        }
    }
    for key in meta.driver_prefix_keys(MANIFEST_PREFIX)? {
        if let Some(manifest) = meta.get_driver_value(&key)?.as_deref().and_then(Manifest::decode) {
            digests.extend(manifest_descriptors(&manifest.bytes).0);
        }
    }
    for key in meta.driver_prefix_keys(REFERRER_PREFIX)? {
        if let Some((subject, referrer)) = split_referrer_key(&key) {
            digests.insert(subject.to_owned());
            digests.insert(referrer.to_owned());
        }
    }
    Ok(digests)
}

/// Drop this index+repo's own pointers to `digest`: tags whose target is it (with their freshness
/// records) and referrer records naming it as subject or as referrer. Report their counts separately
/// because only tag removal invalidates search.
///
/// # Errors
/// Returns a store error if a scan or write fails.
pub fn delete_repo_tags_to(
    meta: &MetaStore,
    index: &str,
    repo: &str,
    digest: &str,
) -> Result<(usize, usize), MetaError> {
    let tags = tag_prefix(index, repo);
    meta.commit_driver_txn(|txn| {
        let mut removed_tags = 0;
        for (key, target) in txn.prefix(&tags)? {
            if target == digest.as_bytes()
                && let Some(tag) = key.strip_prefix(tags.as_str())
            {
                txn.remove(&key)?;
                txn.remove(&tag_freshness_key(index, repo, tag))?;
                removed_tags += 1;
            }
        }
        let mut removed_referrers = 0;
        for key in txn.prefix_keys(&format!("{REFERRER_PREFIX}{index}\u{0}{repo}\u{0}"))? {
            if let Some((subject, referrer)) = split_referrer_key(&key)
                && (subject == digest || referrer == digest)
            {
                txn.remove(&key)?;
                removed_referrers += 1;
            }
        }
        Ok(((removed_tags, removed_referrers), Vec::new()))
    })
}

/// The `(subject, referrer)` digests a referrer key `oci\0r\0{index}\0{repo}\0{subject}\0{referrer}`
/// carries: both are manifest digests free of the `\0` separator, so the last two segments name them.
fn split_referrer_key(key: &str) -> Option<(&str, &str)> {
    let (rest, referrer) = key.rsplit_once('\u{0}')?;
    let (_, subject) = rest.rsplit_once('\u{0}')?;
    Some((subject, referrer))
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

    #[test]
    fn test_referenced_manifest_digests_unions_tags_children_and_referrers() {
        let (_dir, meta) = store();
        put_tag(&meta, "hub", "nginx", "latest", "sha256:a").unwrap();
        put_tag(&meta, "store", "app", "v1", "sha256:b").unwrap();
        put_manifest(
            &meta,
            "sha256:idx",
            &Manifest {
                media_type: "application/vnd.oci.image.index.v1+json".to_owned(),
                bytes: br#"{"manifests":[{"digest":"sha256:c"}]}"#.to_vec(),
            },
        )
        .unwrap();
        put_referrer(&meta, "store", "app", "sha256:s", "sha256:r", b"{}").unwrap();
        // A non-utf8 tag target and a corrupt manifest record contribute nothing.
        meta.put_driver_value(&tag_key("hub", "nginx", "bad"), &[0xff]).unwrap();
        meta.put_driver_value(&manifest_key("sha256:corrupt"), &[0x00]).unwrap();

        assert_eq!(
            referenced_manifest_digests(&meta).unwrap(),
            ["sha256:a", "sha256:b", "sha256:c", "sha256:r", "sha256:s"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn test_delete_repo_tags_to_clears_this_repos_tags_and_referrers_only() {
        let (_dir, meta) = store();
        put_tag(&meta, "store", "app", "latest", "sha256:x").unwrap();
        set_tag_freshness(&meta, "store", "app", "latest", "sha256:x", 1).unwrap();
        put_tag(&meta, "store", "app", "other", "sha256:y").unwrap();
        put_tag(&meta, "store", "app2", "keep", "sha256:x").unwrap();
        put_referrer(&meta, "store", "app", "sha256:x", "sha256:ref", b"{}").unwrap();
        put_referrer(&meta, "store", "app", "sha256:sub", "sha256:x", b"{}").unwrap();
        put_referrer(&meta, "store", "app", "sha256:z", "sha256:zref", b"{}").unwrap();

        assert_eq!(delete_repo_tags_to(&meta, "store", "app", "sha256:x").unwrap(), (1, 2));
        assert_eq!(get_tag(&meta, "store", "app", "latest").unwrap(), None);
        assert_eq!(tag_freshness(&meta, "store", "app", "latest").unwrap(), None);
        assert_eq!(
            get_tag(&meta, "store", "app", "other").unwrap(),
            Some("sha256:y".to_owned())
        );
        assert_eq!(
            get_tag(&meta, "store", "app2", "keep").unwrap(),
            Some("sha256:x".to_owned())
        );
        assert!(list_referrers(&meta, "store", "app", "sha256:x").unwrap().is_empty());
        assert!(list_referrers(&meta, "store", "app", "sha256:sub").unwrap().is_empty());
        assert_eq!(list_referrers(&meta, "store", "app", "sha256:z").unwrap().len(), 1);
        assert_eq!(delete_repo_tags_to(&meta, "store", "app", "sha256:x").unwrap(), (0, 0));
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
    fn test_blob_membership_delete_reports_presence() {
        let (_dir, meta, digest) = blob_member();

        assert_eq!(
            (
                delete_blob_membership(&meta, "store", "app", &digest).unwrap(),
                delete_blob_membership(&meta, "store", "app", &digest).unwrap(),
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

    fn blob_member() -> (tempfile::TempDir, MetaStore, String) {
        let (dir, meta) = store();
        let digest = format!("sha256:{}", "a".repeat(64));
        record_blob_membership(&meta, "store", "app", &digest).unwrap();
        (dir, meta, digest)
    }

    #[test]
    fn test_prune_membership_keeps_an_index_child_and_drops_the_unreferenced() {
        let (_dir, meta) = store();
        let child = format!("sha256:{}", "c".repeat(64));
        let plain = format!("sha256:{}", "d".repeat(64));
        // The index names `child` but the child's own bytes are never stored, so its membership marker
        // has no manifest behind it; `plain` is a stored manifest nothing names.
        record_manifest(&meta, "store", "app", "sha256:idx", &index_of(&child)).unwrap();
        record_manifest(
            &meta,
            "store",
            "app",
            &plain,
            &Manifest {
                media_type: "application/vnd.oci.image.manifest.v1+json".to_owned(),
                bytes: b"{\"schemaVersion\":2}".to_vec(),
            },
        )
        .unwrap();

        prune_manifest_membership(&meta, "store", "app", &child).unwrap();
        prune_manifest_membership(&meta, "store", "app", &plain).unwrap();
        // The child stays reachable through the index that names it; the plain manifest is dropped.
        assert!(manifest_is_member(&meta, "store", "app", &child).unwrap());
        assert!(!manifest_is_member(&meta, "store", "app", &plain).unwrap());
    }

    #[test]
    fn test_split_referrer_key_needs_two_separators() {
        assert_eq!(split_referrer_key("nodelim"), None);
        assert_eq!(split_referrer_key("only\u{0}one"), None);
        assert_eq!(split_referrer_key("a\u{0}b\u{0}c"), Some(("b", "c")));
    }
}
