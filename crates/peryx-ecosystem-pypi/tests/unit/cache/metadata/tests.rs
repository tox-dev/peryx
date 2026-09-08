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
    state.meta.put_metadata(digest.as_str(), &"f".repeat(64)).unwrap();

    let bytes = metadata_bytes(
        &state,
        state.index_at(0),
        &digest,
        "pypi",
        "pkg-1.0-py3-none-any.whl.metadata",
    )
    .await
    .unwrap();

    assert_eq!(&bytes[..], b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
    assert!(state.meta.get_metadata_digest(digest.as_str()).unwrap().is_some());
}

#[tokio::test]
async fn test_metadata_backfill_candidates_skip_existing_and_successful_records() {
    let (_dir, state) = test_state();
    let existing = Digest::of(b"existing");
    state.meta.put_metadata(existing.as_str(), &"e".repeat(64)).unwrap();
    let wheel = test_wheel(b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n");
    let digest = state.blobs.put_bytes(&wheel).await.unwrap();

    run_metadata_backfill_candidates(
        state.clone(),
        "pypi".to_owned(),
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

    assert!(state.meta.get_metadata_digest(digest.as_str()).unwrap().is_some());
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
        "pypi".to_owned(),
        &[
            registration("broken-1.0-py3-none-any.whl", &unfetchable),
            registration("pkg-1.0-py3-none-any.whl", &cached),
        ],
    );

    drain(state.plugin_service::<MetadataBackfills>().unwrap()).await;
    assert!(state.meta.get_metadata_digest(cached.as_str()).unwrap().is_some());
    assert!(state.meta.get_metadata_digest(unfetchable.as_str()).unwrap().is_none());
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
        "pypi".to_owned(),
        &[registration("pkg-1.0-py3-none-any.whl", &digest)],
    );
    drain(backfills).await;

    assert!(state.meta.get_metadata_digest(digest.as_str()).unwrap().is_none());
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
        "pypi".to_owned(),
        &[registration("pkg-1.0-py3-none-any.whl", &digest)],
    );
    drain(backfills).await;

    assert!(state.meta.get_metadata_digest(digest.as_str()).unwrap().is_some());
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
    state_with(vec![index("pypi", upstream_kind())])
}

fn index(name: &str, kind: IndexKind) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind,
        policy: peryx_policy::Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    }
}

fn upstream_kind() -> IndexKind {
    IndexKind::Cached {
        client: UpstreamClient::new("https://pypi.org/simple/").unwrap(),
        offline: false,
    }
}

fn state_with(indexes: Vec<Index>) -> (tempfile::TempDir, Arc<ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = peryx_driver::AppState::new(meta, blobs, 60, indexes);
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

const WHEEL: &str = "pkg-1.0-py3-none-any.whl";
const EXTRACTED: &[u8] = b"Metadata-Version: 2.1\nName: pkg\nVersion: 1.0\n";

async fn put_sidecar(state: &ServingState, bytes: &[u8]) -> Digest {
    state.blobs.put_bytes(bytes).await.unwrap()
}

fn claim(state: &ServingState, index: &str, artifact: &Digest, sidecar: &Digest) {
    crate::tests::register_publication(
        &state.meta,
        index,
        WHEEL,
        artifact.as_str(),
        Some((&format!("https://{index}.example/{WHEEL}.metadata"), sidecar.as_str())),
    );
}

#[tokio::test]
async fn test_metadata_serves_each_cached_index_its_own_sidecar_for_one_digest() {
    let (_dir, state) = state_with(vec![index("first", upstream_kind()), index("second", upstream_kind())]);
    let artifact = state.blobs.put_bytes(&test_wheel(EXTRACTED)).await.unwrap();
    let first = put_sidecar(&state, b"Metadata-Version: 2.1\nName: pkg\nSummary: first\n").await;
    let second = put_sidecar(&state, b"Metadata-Version: 2.1\nName: pkg\nSummary: second\n").await;
    claim(&state, "first", &artifact, &first);
    claim(&state, "second", &artifact, &second);

    let served = |position| {
        let state = state.clone();
        let artifact = artifact.clone();
        async move {
            metadata_bytes(
                &state,
                state.index_at(position),
                &artifact,
                state.index_at(position).route.as_str(),
                &format!("{WHEEL}.metadata"),
            )
            .await
            .unwrap()
        }
    };

    assert_eq!(
        &served(0).await[..],
        b"Metadata-Version: 2.1\nName: pkg\nSummary: first\n"
    );
    assert_eq!(
        &served(1).await[..],
        b"Metadata-Version: 2.1\nName: pkg\nSummary: second\n"
    );
}

#[tokio::test]
async fn test_metadata_on_a_publication_without_a_claim_inherits_no_sidecar() {
    let (_dir, state) = state_with(vec![index("claiming", upstream_kind()), index("bare", upstream_kind())]);
    let artifact = state.blobs.put_bytes(&test_wheel(EXTRACTED)).await.unwrap();
    let sidecar = put_sidecar(&state, b"Metadata-Version: 2.1\nName: pkg\nSummary: borrowed\n").await;
    claim(&state, "claiming", &artifact, &sidecar);
    crate::tests::register_publication(&state.meta, "bare", WHEEL, artifact.as_str(), None);

    let bytes = metadata_bytes(
        &state,
        state.index_at(1),
        &artifact,
        "bare",
        &format!("{WHEEL}.metadata"),
    )
    .await
    .unwrap();

    assert_eq!(
        &bytes[..],
        EXTRACTED,
        "the bare publication falls back to the artifact's own metadata"
    );
}

#[tokio::test]
async fn test_generated_metadata_stays_shared_by_digest_across_publications() {
    let (_dir, state) = state_with(vec![index("first", upstream_kind()), index("second", upstream_kind())]);
    let artifact = state.blobs.put_bytes(&test_wheel(EXTRACTED)).await.unwrap();
    crate::tests::register_publication(&state.meta, "first", WHEEL, artifact.as_str(), None);
    crate::tests::register_publication(&state.meta, "second", WHEEL, artifact.as_str(), None);
    let first = metadata_bytes(
        &state,
        state.index_at(0),
        &artifact,
        "first",
        &format!("{WHEEL}.metadata"),
    )
    .await
    .unwrap();
    let generated = state.meta.get_metadata_digest(artifact.as_str()).unwrap().unwrap();
    assert!(state.blobs.delete(&artifact).await.unwrap());

    let second = metadata_bytes(
        &state,
        state.index_at(1),
        &artifact,
        "second",
        &format!("{WHEEL}.metadata"),
    )
    .await
    .unwrap();

    assert_eq!(first, second);
    assert_eq!(generated, Digest::of(EXTRACTED).as_str());
}

fn hosted_upload(state: &ServingState, index: &str, artifact: &Digest, trashed: Option<peryx_core::TrashInfo>) {
    let uploaded = crate::upload::Uploaded {
        version: "1.0".to_owned(),
        file: crate::File {
            filename: WHEEL.to_owned(),
            url: format!("/{index}/files/{}/{WHEEL}", artifact.as_str()),
            hashes: std::collections::BTreeMap::from([("sha256".to_owned(), artifact.as_str().to_owned())]),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: crate::Yanked::No,
            core_metadata: crate::CoreMetadata::Absent,
            dist_info_metadata: crate::CoreMetadata::Absent,
            gpg_sig: None,
            provenance: crate::Provenance::Absent,
        },
        trashed,
    };
    state
        .meta
        .put_upload(index, "pkg", WHEEL, crate::to_json(&uploaded).as_bytes())
        .unwrap();
}

fn overlay_state() -> (tempfile::TempDir, Arc<ServingState>) {
    state_with(vec![
        index("hosted", IndexKind::Hosted { volatile: false }),
        index("proxy", upstream_kind()),
        index(
            "overlay",
            IndexKind::Virtual {
                layers: vec![0, 1],
                write_target: Some(0),
            },
        ),
    ])
}

async fn overlay_metadata(state: &Arc<ServingState>, artifact: &Digest) -> Result<Bytes, CacheError> {
    metadata_bytes(
        state,
        state.index_at(2),
        artifact,
        "overlay",
        &format!("{WHEEL}.metadata"),
    )
    .await
}

#[tokio::test]
async fn test_virtual_index_stops_at_the_hosted_layer_that_owns_the_file() {
    let (_dir, state) = overlay_state();
    let artifact = state.blobs.put_bytes(&test_wheel(EXTRACTED)).await.unwrap();
    let sidecar = put_sidecar(&state, b"Metadata-Version: 2.1\nName: pkg\nSummary: proxied\n").await;
    claim(&state, "proxy", &artifact, &sidecar);
    hosted_upload(&state, "hosted", &artifact, None);

    let bytes = overlay_metadata(&state, &artifact).await.unwrap();

    assert_eq!(&bytes[..], EXTRACTED, "the hosted publication lends no proxied claim");
}

enum HostedRow {
    Absent,
    Trashed,
    OtherDigest,
}

#[rstest::rstest]
#[case::no_upload(HostedRow::Absent)]
#[case::trashed(HostedRow::Trashed)]
#[case::other_digest(HostedRow::OtherDigest)]
#[tokio::test]
async fn test_virtual_index_falls_through_a_hosted_layer_that_does_not_publish_the_file(#[case] hosted: HostedRow) {
    let (_dir, state) = overlay_state();
    let artifact = state.blobs.put_bytes(&test_wheel(EXTRACTED)).await.unwrap();
    let sidecar = put_sidecar(&state, b"Metadata-Version: 2.1\nName: pkg\nSummary: proxied\n").await;
    claim(&state, "proxy", &artifact, &sidecar);
    match hosted {
        HostedRow::Absent => {}
        HostedRow::Trashed => hosted_upload(
            &state,
            "hosted",
            &artifact,
            Some(peryx_core::TrashInfo {
                deleted_at_unix: 1,
                actor: None,
                reason: None,
            }),
        ),
        HostedRow::OtherDigest => hosted_upload(&state, "hosted", &Digest::of(b"another wheel"), None),
    }

    let bytes = overlay_metadata(&state, &artifact).await.unwrap();

    assert_eq!(&bytes[..], b"Metadata-Version: 2.1\nName: pkg\nSummary: proxied\n");
}

#[tokio::test]
async fn test_virtual_index_layer_cycle_resolves_no_publication() {
    let (_dir, state) = state_with(vec![index(
        "loop",
        IndexKind::Virtual {
            layers: vec![0],
            write_target: None,
        },
    )]);
    let artifact = state.blobs.put_bytes(&test_wheel(EXTRACTED)).await.unwrap();

    let bytes = metadata_bytes(
        &state,
        state.index_at(0),
        &artifact,
        "loop",
        &format!("{WHEEL}.metadata"),
    )
    .await
    .unwrap();

    assert_eq!(&bytes[..], EXTRACTED);
}

#[rstest::rstest]
#[case::claimed_sidecar(true)]
#[case::derived_record(false)]
#[tokio::test]
async fn test_metadata_rejects_a_record_naming_a_malformed_digest(#[case] claimed: bool) {
    let (_dir, state) = test_state();
    let artifact = state.blobs.put_bytes(&test_wheel(EXTRACTED)).await.unwrap();
    if claimed {
        crate::tests::register_publication(
            &state.meta,
            "pypi",
            WHEEL,
            artifact.as_str(),
            Some(("https://pypi.example/sidecar", "not-hex")),
        );
    } else {
        state.meta.put_metadata(artifact.as_str(), "not-hex").unwrap();
    }

    let err = metadata_bytes(
        &state,
        state.index_at(0),
        &artifact,
        "pypi",
        &format!("{WHEEL}.metadata"),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, CacheError::FileNotFound));
}

#[tokio::test]
async fn test_virtual_index_surfaces_an_undecodable_hosted_record() {
    let (_dir, state) = overlay_state();
    let artifact = state.blobs.put_bytes(&test_wheel(EXTRACTED)).await.unwrap();
    state.meta.put_upload("hosted", "pkg", WHEEL, b"not json").unwrap();

    let err = overlay_metadata(&state, &artifact).await.unwrap_err();

    assert!(matches!(err, CacheError::Parse(_)), "{err:?}");
}

const RANGED_WHEEL: &str = "sample_pkg-1.0-py3-none-any.whl";
const RANGED_METADATA: &str = "Metadata-Version: 2.1\nName: sample-pkg\nVersion: 1.0\n";

fn ranged_wheel_bytes() -> Vec<u8> {
    ranged_wheel_holding(RANGED_METADATA.as_bytes(), zip::CompressionMethod::Deflated)
}

fn ranged_wheel_holding(metadata: &[u8], method: zip::CompressionMethod) -> Vec<u8> {
    use std::io::Write as _;
    let mut bytes = Vec::new();
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
    let options = zip::write::SimpleFileOptions::default().compression_method(method);
    archive.start_file("sample_pkg/__init__.py", options).unwrap();
    archive.write_all(b"__version__ = \"1.0\"\n").unwrap();
    archive.start_file("sample_pkg-1.0.dist-info/METADATA", options).unwrap();
    archive.write_all(metadata).unwrap();
    archive.start_file("sample_pkg-1.0.dist-info/WHEEL", options).unwrap();
    archive
        .write_all(b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n")
        .unwrap();
    archive.finish().unwrap();
    bytes
}

async fn ranged_outcome(wheel: Vec<u8>) -> RemoteMetadata {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path(format!("/files/{RANGED_WHEEL}")))
        .respond_with(RangedWheelServer(wheel))
        .mount(&server)
        .await;
    let client = ArtifactClient::from(UpstreamClient::new(&format!("{}/", server.uri())).unwrap());
    wheel_metadata_by_range(
        &client,
        &format!("{}/files/{RANGED_WHEEL}", server.uri()),
        RANGED_WHEEL,
    )
    .await
    .unwrap()
}

/// The member budget admits a member of exactly its size and refuses only what passes it, and it asks
/// that of both sizes a ZIP records: what the member occupies in the archive and what it becomes when
/// decoded. Either one over the budget is enough to decline the ranged read, so a member that
/// compresses small still cannot smuggle a large one past.
///
/// Only a member at the limit separates admitting from refusing it, which is why each case is exact.
#[tokio::test]
async fn test_a_member_of_exactly_the_budget_is_read() {
    let metadata = vec![b'm'; usize::try_from(crate::archive::MAX_WHEEL_METADATA_BYTES).unwrap()];
    let outcome = ranged_outcome(ranged_wheel_holding(&metadata, zip::CompressionMethod::Stored)).await;

    let RemoteMetadata::Found(read) = outcome else {
        panic!("a member at the budget is within it");
    };
    assert_eq!(read.len(), metadata.len());
}

#[tokio::test]
async fn test_a_member_one_byte_past_the_budget_is_declined() {
    let metadata = vec![b'm'; usize::try_from(crate::archive::MAX_WHEEL_METADATA_BYTES).unwrap() + 1];
    let outcome = ranged_outcome(ranged_wheel_holding(&metadata, zip::CompressionMethod::Stored)).await;

    assert!(matches!(outcome, RemoteMetadata::Unsupported));
}

#[tokio::test]
async fn test_a_member_that_compresses_small_is_still_judged_by_its_decoded_size() {
    let metadata = vec![b'm'; usize::try_from(crate::archive::MAX_WHEEL_METADATA_BYTES).unwrap() + 1];
    let wheel = ranged_wheel_holding(&metadata, zip::CompressionMethod::Deflated);
    assert!(wheel.len() < metadata.len(), "the member has to compress well for this to mean anything");

    assert!(matches!(ranged_outcome(wheel).await, RemoteMetadata::Unsupported));
}

/// A server that answers the ranged read the way a real one does: a `HEAD` carrying the length and a
/// strong validator, then each `GET` returning exactly the bytes its `Range` asked for.
struct RangedWheelServer(Vec<u8>);

impl wiremock::Respond for RangedWheelServer {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let etag = "\"sample-pkg-1.0\"";
        let Some(range) = request.headers.get("range") else {
            return wiremock::ResponseTemplate::new(200)
                .insert_header("etag", etag)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", self.0.len().to_string().as_str());
        };
        let spec = range.to_str().unwrap().trim_start_matches("bytes=").to_owned();
        let (start, end) = spec.split_once('-').expect("a bounded range");
        let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
        wiremock::ResponseTemplate::new(206)
            .insert_header("etag", etag)
            .insert_header(
                "content-range",
                format!("bytes {start}-{end}/{}", self.0.len()).as_str(),
            )
            .set_body_bytes(self.0[start..=end].to_vec())
    }
}

/// Reading a wheel's metadata over ranges is arithmetic on offsets and lengths: where the central
/// directory starts and ends, where the member's data begins, and how far it runs. Every one of those
/// ends is inclusive, so each is a length short of the next offset. Getting any of them wrong asks the
/// server for the wrong bytes, and the answer is either a refused range or a member that will not
/// decode, never the metadata.
///
/// Nothing drove that arithmetic before: the only ranged-read test rejected bad filenames without
/// fetching anything.
#[tokio::test]
async fn test_a_wheel_member_is_read_back_whole_over_ranges() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path(format!("/files/{RANGED_WHEEL}")))
        .respond_with(RangedWheelServer(ranged_wheel_bytes()))
        .mount(&server)
        .await;
    let client = ArtifactClient::from(UpstreamClient::new(&format!("{}/", server.uri())).unwrap());

    let outcome = wheel_metadata_by_range(
        &client,
        &format!("{}/files/{RANGED_WHEEL}", server.uri()),
        RANGED_WHEEL,
    )
    .await
    .unwrap();

    let RemoteMetadata::Found(metadata) = outcome else {
        panic!("the member is present, so the ranged read finds it");
    };
    assert_eq!(String::from_utf8(metadata).unwrap(), RANGED_METADATA);
}
