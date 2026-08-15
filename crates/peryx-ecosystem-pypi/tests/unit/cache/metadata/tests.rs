use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::*;
use peryx_upstream::UpstreamClient;

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
    spawn_metadata_backfill(
        &state,
        "pypi".to_owned(),
        &[
            registration("broken-1.0-py3-none-any.whl", &unfetchable),
            registration("pkg-1.0-py3-none-any.whl", &cached),
        ],
    );

    drain(state.plugin_service::<MetadataBackfills>().unwrap()).await;
    assert!(state.meta.get_metadata(cached.as_str()).unwrap().is_some());
    assert!(state.meta.get_metadata(unfetchable.as_str()).unwrap().is_none());
}

#[tokio::test]
async fn test_metadata_backfill_rejects_work_before_spawning_when_capacity_is_full() {
    let (_dir, state) = test_state();
    let backfills = state.plugin_service::<MetadataBackfills>().unwrap();
    let slots = [
        backfills.slots.clone().try_acquire_owned().unwrap(),
        backfills.slots.clone().try_acquire_owned().unwrap(),
    ];
    let wheel = test_wheel(b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
    let digest = state.blobs.put_bytes(&wheel).await.unwrap();

    spawn_metadata_backfill(
        &state,
        "pypi".to_owned(),
        &[registration("pkg-1.0-py3-none-any.whl", &digest)],
    );
    drain(backfills).await;

    assert!(state.meta.get_metadata(digest.as_str()).unwrap().is_none());
    drop(slots);
}

#[tokio::test]
async fn test_metadata_backfill_reaps_completed_tasks_before_accepting_more() {
    let (_dir, state) = test_state();
    let backfills = state.plugin_service::<MetadataBackfills>().unwrap();
    let (completed, completed_rx) = tokio::sync::oneshot::channel();
    let (failed, failed_rx) = tokio::sync::oneshot::channel();
    {
        let mut tasks = backfills
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.spawn(async move {
            completed.send(()).unwrap();
        });
        tasks.spawn(async move {
            failed.send(()).unwrap();
            panic!("test backfill failure");
        });
    }
    completed_rx.await.unwrap();
    failed_rx.await.unwrap();
    let wheel = test_wheel(b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
    let digest = state.blobs.put_bytes(&wheel).await.unwrap();

    spawn_metadata_backfill(
        &state,
        "pypi".to_owned(),
        &[registration("pkg-1.0-py3-none-any.whl", &digest)],
    );
    drain(backfills).await;

    assert!(state.meta.get_metadata(digest.as_str()).unwrap().is_some());
}

#[rstest::rstest]
#[case::completed(false)]
#[case::owner_dropped(true)]
#[tokio::test]
async fn test_metadata_backfill_tasks_finish(#[case] drop_owner: bool) {
    let backfills = MetadataBackfills::default();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (completed, completed_rx) = tokio::sync::oneshot::channel::<()>();
    let task = backfills
        .tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .spawn(async move {
            started.send(()).unwrap();
            drop(release_rx.await);
            completed.send(()).unwrap();
        });
    started_rx.await.unwrap();

    if drop_owner {
        drop(backfills);
        drop(release);
    } else {
        release.send(()).unwrap();
        drain(&backfills).await;
    }
    let _ = completed_rx.await;

    assert!(task.is_finished());
}

async fn drain(backfills: &MetadataBackfills) {
    let mut tasks = {
        let mut owned = backfills
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *owned)
    };
    while let Some(result) = tasks.join_next().await {
        result.expect("metadata backfill task does not panic");
    }
}

fn test_state() -> (tempfile::TempDir, Arc<ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = peryx_driver::AppState::new(meta, blobs, 60, Vec::new());
    peryx_plugin_registry::PluginRegistry::new(vec![crate::registration()])
        .unwrap()
        .activate([crate::ECOSYSTEM])
        .unwrap()
        .install_drivers(
            &mut state.runtime_install_context().unwrap(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
    (dir, state.serving)
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

fn registration(filename: &str, digest: &Digest) -> Registration {
    Registration {
        filename: filename.to_owned(),
        sha256: digest.as_str().to_owned(),
        url: format!("https://example.invalid/{filename}"),
        size: None,
        metadata: None,
        provenance: None,
    }
}
