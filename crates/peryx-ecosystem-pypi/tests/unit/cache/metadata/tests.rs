use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::*;

#[test]
fn test_metadata_from_artifact_path_skips_unsupported_formats() {
    assert!(
        metadata_from_artifact_path("pkg-1.0.zip", std::path::Path::new("unused"))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_wheel_metadata_by_range_rejects_invalid_names_before_fetch() {
    let client = ArtifactClient::from(UpstreamClient::new("https://pypi.org/simple/").unwrap());

    assert!(matches!(
        wheel_metadata_by_range(&client, "https://example.invalid/pkg.zip", "pkg-1.0.zip").await,
        Ok(RemoteMetadata::Unsupported)
    ));
    assert!(matches!(
        wheel_metadata_by_range(&client, "https://example.invalid/pkg.whl", "pkg.whl").await,
        Err(RangeError::Invalid(_))
    ));
}

#[tokio::test]
async fn test_metadata_bytes_regenerates_missing_generated_blob() {
    let (_dir, state) = test_state();
    let wheel = test_wheel(b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
    let digest = state.blobs.put_bytes(&wheel).await.unwrap();
    state
        .meta
        .put_metadata(
            digest.as_str(),
            GENERATED_METADATA_URL,
            &"f".repeat(64),
            GENERATED_METADATA_URL,
        )
        .unwrap();

    let bytes = metadata_bytes(&state, &digest, "pypi", "pkg-1.0-py3-none-any.whl.metadata")
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
    assert!(state.meta.get_metadata(digest.as_str()).unwrap().is_some());
}

#[tokio::test]
async fn test_metadata_backfill_candidates_skip_existing_and_successful_records() {
    let (_dir, state) = test_state();
    let existing = Digest::of(b"existing");
    state
        .meta
        .put_metadata(
            existing.as_str(),
            GENERATED_METADATA_URL,
            &"e".repeat(64),
            GENERATED_METADATA_URL,
        )
        .unwrap();
    let wheel = test_wheel(b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
    let digest = state.blobs.put_bytes(&wheel).await.unwrap();

    run_metadata_backfill_candidates(
        state.clone(),
        "pypi".to_owned(),
        vec![
            MetadataBackfillCandidate {
                digest: existing,
                filename: "pkg-1.0-py3-none-any.whl".to_owned(),
            },
            MetadataBackfillCandidate {
                digest: digest.clone(),
                filename: "pkg-1.0-py3-none-any.whl".to_owned(),
            },
        ],
    )
    .await;

    assert!(state.meta.get_metadata(digest.as_str()).unwrap().is_some());
}

#[tokio::test]
async fn test_spawn_metadata_backfill_synthesizes_registered_wheels_and_logs_failures() {
    let (_dir, state) = test_state();
    let unfetchable = Digest::of(b"unfetchable");
    let wheel = test_wheel(b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
    let cached = state.blobs.put_bytes(&wheel).await.unwrap();
    // The first candidate has no stored blob and no file URL, so its synthesis fails and is
    // logged; the second reads its cached blob and registers. Both are wheels advertising no
    // metadata, so the candidate filter keeps them. The candidates run in order, so polling the
    // second awaits the spawned task past the first.
    spawn_metadata_backfill(
        state.clone(),
        "pypi".to_owned(),
        &[
            Registration {
                filename: "broken-1.0-py3-none-any.whl".to_owned(),
                sha256: unfetchable.as_str().to_owned(),
                url: "https://example.invalid/broken.whl".to_owned(),
                size: None,
                metadata: None,
                provenance: None,
            },
            Registration {
                filename: "pkg-1.0-py3-none-any.whl".to_owned(),
                sha256: cached.as_str().to_owned(),
                url: "https://example.invalid/pkg.whl".to_owned(),
                size: None,
                metadata: None,
                provenance: None,
            },
        ],
    );

    let mut registered = None;
    for _ in 0..1000 {
        if let Some(record) = state.meta.get_metadata(cached.as_str()).unwrap() {
            registered = Some(record);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(
        registered.is_some(),
        "the spawned backfill registers the cached wheel's metadata"
    );
    assert!(state.meta.get_metadata(unfetchable.as_str()).unwrap().is_none());
}

fn test_state() -> (tempfile::TempDir, Arc<ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, peryx_driver::AppState::new(meta, blobs, 60, Vec::new()).serving)
}

fn test_wheel(metadata: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("pkg-1.0.dist-info/METADATA", options).unwrap();
        std::io::Write::write_all(&mut zip, metadata).unwrap();
        zip.finish().unwrap();
    }
    bytes
}
