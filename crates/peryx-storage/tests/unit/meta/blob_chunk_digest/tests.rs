use peryx_identity::ArtifactDigest;

use crate::blob::{CHUNK_BYTES, ChunkedDigest, Digest};
use crate::meta::MetaStore;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn artifact(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256(Digest::of(bytes).as_str()).unwrap()
}

#[test]
fn test_a_recorded_chunk_digest_reads_back_identically() {
    let (_dir, meta) = store();
    let bytes = b"the very large release archive bytes";
    let digest = artifact(bytes);
    let chunked = ChunkedDigest::of(bytes, CHUNK_BYTES);

    meta.put_blob_chunk_digest(&digest, &chunked).unwrap();

    assert_eq!(meta.blob_chunk_digest(&digest).unwrap(), Some(chunked));
}

#[test]
fn test_an_uncatalogued_digest_reads_back_none() {
    let (_dir, meta) = store();

    assert_eq!(meta.blob_chunk_digest(&artifact(b"never catalogued")).unwrap(), None);
}

#[test]
fn test_a_later_record_overwrites_the_prior_chunk_digest() {
    let (_dir, meta) = store();
    let digest = artifact(b"same content address");
    let first = ChunkedDigest::of(b"aaaa", std::num::NonZeroU64::new(2).unwrap());
    let second = ChunkedDigest::of(b"bbbbbb", std::num::NonZeroU64::new(3).unwrap());

    meta.put_blob_chunk_digest(&digest, &first).unwrap();
    meta.put_blob_chunk_digest(&digest, &second).unwrap();

    assert_eq!(meta.blob_chunk_digest(&digest).unwrap(), Some(second));
}
