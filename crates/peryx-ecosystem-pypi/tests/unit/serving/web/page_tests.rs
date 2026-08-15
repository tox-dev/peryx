use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::BrowseSection;
use peryx_driver::AppState;
use peryx_driver::serving::BrowseDriver as _;
use peryx_ha::{ArtifactPlacement, ArtifactSource};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use super::PypiServing;
use crate::store::PypiStore as _;
use crate::upload::Uploaded;
use crate::{CoreMetadata, File, Provenance, Yanked};

const FILENAME: &str = "demo-1.0-py3-none-any.whl";

#[tokio::test]
async fn project_page_converts_metadata_lifecycle_and_provenance() {
    let (_directory, state) = rich_project();
    let page = PypiServing
        .browse(
            state.serving.clone(),
            0,
            "index=hosted&project=demo&version=1.0&filename=py3.*whl&filename_match=regex".to_owned(),
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        (page.title.as_str(), page.subtitle.as_deref(), page.summary.as_deref()),
        ("Demo", Some("1.0"), Some("A demo package"))
    );
    assert!(matches!(
        &page.sections[0],
        BrowseSection::Markup { heading, html, notice }
            if heading == "Description" && html.contains("About demo") && notice.is_none()
    ));
    assert!(matches!(
        &page.sections[1],
        BrowseSection::Properties { heading, entries }
            if heading == "Requires Python"
                && entries.len() == 1
                && entries[0].label == "Requires Python"
                && entries[0].value == ">=3.11"
    ));
    assert!(matches!(
        &page.sections[2],
        BrowseSection::Properties { heading, entries }
            if heading == "Keywords"
                && entries.iter().map(|entry| entry.value.as_str()).collect::<Vec<_>>() == ["build", "serve"]
    ));
    assert!(matches!(
        &page.sections[3],
        BrowseSection::Links { heading, entries, empty }
            if heading == "Links"
                && entries.len() == 1
                && entries[0].label == "Documentation"
                && entries[0].href == "https://example.com/docs"
                && empty.is_empty()
    ));
    assert!(matches!(
        &page.sections[4],
        BrowseSection::Properties { heading, entries }
            if heading == "Classifiers"
                && entries.len() == 1
                && entries[0].label == "Topic"
                && entries[0].value == "Software Development, Utilities"
    ));
    assert!(matches!(
        &page.sections[5],
        BrowseSection::Table { heading, rows, .. }
            if heading == "Releases"
                && rows.len() == 1
                && rows[0].badges.len() == 1
                && rows[0].badges[0].hint.as_deref() == Some("security issue")
    ));
    assert!(matches!(
        &page.sections[6],
        BrowseSection::Table { heading, rows, .. }
            if heading == "Files"
                && rows.len() == 1
                && rows[0].cells[0].href.as_deref()
                    == Some("/browse?index=hosted&project=Demo&sha256=c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c&file=demo-1.0-py3-none-any.whl")
                && rows[0].badges.iter().any(|badge| {
                    badge.label == "hosted provenance"
                        && badge.class == "provenance-valid"
                        && badge.hint.as_deref()
                            == Some("matched: matched; mismatched: mismatched; unknown")
                })
    ));
}

#[tokio::test]
async fn project_page_supports_substring_filters() {
    let (_directory, state) = rich_project();
    let page = PypiServing
        .browse(
            state.serving.clone(),
            0,
            "index=hosted&project=demo&filename=PY3-NONE".to_owned(),
        )
        .await
        .unwrap()
        .unwrap();

    assert!(matches!(
        page.sections.last(),
        Some(BrowseSection::Table { heading, rows, .. }) if heading == "Files" && rows.len() == 1
    ));
}

#[tokio::test]
async fn project_page_rejects_invalid_filename_regexes() {
    let (_directory, state) = rich_project();
    let error = PypiServing
        .browse(
            state.serving.clone(),
            0,
            "index=hosted&project=demo&filename=%5B&filename_match=regex".to_owned(),
        )
        .await
        .unwrap_err();

    assert!(error.starts_with("invalid regex: regex parse error:"), "{error}");
}

fn rich_project() -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        vec![Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    crate::tests::install(&mut state);

    let artifact = Digest::of(b"artifact");
    let metadata = b"Metadata-Version: 2.5\nName: Demo\nVersion: 1.0\nSummary: A demo package\nRequires-Python: >=3.11\nKeywords: build, serve\nProject-URL: docs, https://example.com/docs\nClassifier: Topic :: Software Development\nClassifier: Topic :: Utilities\nDescription-Content-Type: text/markdown\n\n# About demo\n";
    let metadata_digest = Digest::of(metadata);
    state
        .serving
        .blobs
        .blocking()
        .put_bytes_as(metadata, &metadata_digest)
        .unwrap();
    state
        .serving
        .meta
        .put_metadata(artifact.as_str(), "peryx:generated", metadata_digest.as_str(), "hosted")
        .unwrap();

    let provenance = stored_provenance(artifact.as_str());
    let provenance_digest = Digest::of(&provenance);
    state
        .serving
        .blobs
        .blocking()
        .put_bytes_as(&provenance, &provenance_digest)
        .unwrap();
    state
        .serving
        .meta
        .put_provenance(artifact.as_str(), provenance_digest.as_str(), provenance.len() as u64)
        .unwrap();
    state
        .serving
        .meta
        .put_upload(
            "hosted",
            "demo",
            FILENAME,
            crate::to_json(&Uploaded {
                version: "1.0".to_owned(),
                file: File {
                    filename: FILENAME.to_owned(),
                    url: String::new(),
                    hashes: BTreeMap::from([("sha256".to_owned(), artifact.as_str().to_owned())]),
                    requires_python: Some(">=3.11".to_owned()),
                    size: Some(8),
                    upload_time: Some("2026-08-10T00:00:00Z".to_owned()),
                    yanked: Yanked::Reason("security issue".to_owned()),
                    core_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                        "sha256".to_owned(),
                        metadata_digest.as_str().to_owned(),
                    )])),
                    dist_info_metadata: CoreMetadata::Absent,
                    gpg_sig: None,
                    provenance: Provenance::Url("/provenance".to_owned()),
                },
                trashed: None,
            })
            .as_bytes(),
        )
        .unwrap();
    state.serving.meta.put_project("hosted", "demo", "Demo").unwrap();
    state
        .serving
        .meta
        .put_artifact_placement(
            artifact.as_str(),
            &ArtifactPlacement::record(ArtifactSource::Hosted, true),
        )
        .unwrap();
    (directory, Arc::new(state))
}

fn stored_provenance(artifact: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "attestation_bundles": [{
            "publisher": null,
            "attestations": [
                attestation(Some(&statement(FILENAME, artifact, "matched"))),
                attestation(Some(&statement("other.whl", artifact, "mismatched"))),
                attestation(None),
            ],
        }],
    }))
    .unwrap()
}

fn attestation(statement: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "envelope": {
            "statement": statement,
            "signature": "signature",
        },
    })
}

fn statement(filename: &str, artifact: &str, predicate: &str) -> String {
    STANDARD.encode(
        serde_json::json!({
            "subject": [{"name": filename, "digest": {"sha256": artifact}}],
            "predicateType": predicate,
        })
        .to_string(),
    )
}
