//! The harness the search tests build on.

use peryx_identity::IndexAcl;

pub(super) use std::collections::BTreeMap;
pub(super) use std::sync::Arc;

pub(super) use crate::store::CachedIndex;
pub(super) use crate::store::PypiStore as _;
pub(super) use crate::{CoreMetadata, File, Meta, ProjectDetail, Provenance, Yanked, to_json};
pub(super) use axum::http::StatusCode;
pub(super) use peryx_storage::blob::{BlobStore, Digest};
pub(super) use peryx_storage::meta::{MetaError, MetaScanError, MetaStore};
pub(super) use peryx_upstream::UpstreamClient;

pub(super) use crate::cache;
pub(super) use crate::tests::http::{get, harness, harness_with_policies};
pub(super) use crate::upload::Uploaded;
pub(super) use peryx_core::path::local_file_url;
pub(super) use peryx_driver::state::AppState;
pub(super) use peryx_index::{Index, IndexKind};
pub(super) use peryx_policy::{Policy, PolicyConfig};
pub(super) use peryx_search::{PackageSearch, PackageSource, SearchError, SourceFilter};

pub(super) fn policy(configure: impl FnOnce(&mut PolicyConfig)) -> Policy {
    let mut config = PolicyConfig::default();
    configure(&mut config);
    Policy::compile(&config, crate::normalize_name)
}

pub(super) fn put_uploaded_package(
    state: &peryx_driver::state::AppState,
    display: &str,
    normalized: &str,
    summary: &str,
) {
    put_uploaded_package_with_metadata(
        state,
        normalized,
        &format!("Metadata-Version: 2.1\nName: {display}\nVersion: 1.0\nSummary: {summary}\n"),
        None,
    );
    state.meta.put_project("hosted", normalized, display).unwrap();
}

pub(super) fn put_uploaded_package_with_metadata(
    state: &peryx_driver::state::AppState,
    normalized: &str,
    metadata: &str,
    requires_python: Option<&str>,
) {
    let filename = format!("{normalized}-1.0-py3-none-any.whl");
    let artifact_digest = Digest::of(filename.as_bytes());
    let metadata_digest = state.blobs.write(metadata.as_bytes()).unwrap();
    state
        .meta
        .put_metadata(artifact_digest.as_str(), "uploaded", metadata_digest.as_str(), "hosted")
        .unwrap();
    let uploaded = Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: filename.clone(),
            url: local_file_url("hosted", artifact_digest.as_str(), &filename),
            hashes: BTreeMap::from([("sha256".to_owned(), artifact_digest.as_str().to_owned())]),
            requires_python: requires_python.map(str::to_owned),
            size: Some(10),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                "sha256".to_owned(),
                metadata_digest.as_str().to_owned(),
            )])),
            dist_info_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                "sha256".to_owned(),
                metadata_digest.as_str().to_owned(),
            )])),
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    };
    state
        .meta
        .put_upload("hosted", normalized, &filename, to_json(&uploaded).as_bytes())
        .unwrap();
    state.meta.put_project("hosted", normalized, normalized).unwrap();
    state.bump_epoch();
}

pub(super) fn put_cached_package(
    state: &peryx_driver::state::AppState,
    key: &str,
    index: &str,
    normalized: &str,
    detail: &ProjectDetail,
) {
    cache::persist_page(state, key, index, normalized, &cached_index(&to_json(detail))).unwrap();
}

pub(super) fn overlay_state_without_upload() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: UpstreamClient::new("https://example.test/simple/").unwrap(),
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            policy: Policy::default(),
            acl: IndexAcl::default(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![0],
                upload: None,
            },
        },
    ];
    (dir, crate::tests::wired(AppState::new(meta, blobs, 60, indexes)))
}

pub(super) fn file_with_hash(filename: &str, sha256: &str, requires_python: Option<&str>) -> File {
    File {
        filename: filename.to_owned(),
        url: format!("https://files.example/{filename}"),
        hashes: BTreeMap::from([("sha256".to_owned(), sha256.to_owned())]),
        requires_python: requires_python.map(str::to_owned),
        size: Some(10),
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    }
}

pub(super) fn meta_status(status: &str, reason: &str) -> Meta {
    Meta {
        project_status: Some(status.to_owned()),
        project_status_reason: Some(reason.to_owned()),
        ..Meta::default()
    }
}

pub(super) fn cached_index(body: &str) -> CachedIndex {
    CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 1000,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: Some(60),
        body: body.as_bytes().to_vec(),
    }
}

pub(super) fn meta_error() -> MetaError {
    MetaError::Decode(serde_json::from_str::<serde_json::Value>("not json").unwrap_err())
}
