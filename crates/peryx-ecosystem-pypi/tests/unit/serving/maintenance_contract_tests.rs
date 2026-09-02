use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::BrowseSection;
use peryx_core::path::local_artifact_url;
use peryx_driver::serving::{
    BrowseDriver as _, BrowseRequest, CacheDriver as _, IndexCredentialDriver as _, JobConfig, JobDriver as _,
    MetadataRepairDriver as _, NameDriver as _, PolicyDriver as _, ReplicatedApplyDriver as _, TrashDriver as _,
};
use peryx_driver::{AppState, ServingState};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_search::SearchParams;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;

use super::PypiServing;
use crate::store::PypiStore as _;
use crate::upload::Uploaded;
use crate::{CoreMetadata, File, Provenance, Yanked};

#[test]
fn serving_exposes_pypi_maintenance() {
    let (_dir, mut state) = state();
    peryx_plugin_registry::PluginRegistry::new(vec![crate::registration()])
        .unwrap()
        .activate([crate::ECOSYSTEM])
        .unwrap()
        .install_drivers(
            &mut Arc::get_mut(&mut state).unwrap().runtime_install_context().unwrap(),
            &std::collections::HashMap::new(),
        )
        .unwrap();

    assert_eq!(state.idle_reclaimers().count(), 0);
    assert_eq!(
        state
            .intent_finalizers()
            .map(|(ecosystem, _)| ecosystem.clone())
            .collect::<Vec<_>>(),
        vec![crate::ECOSYSTEM]
    );
    assert_eq!(
        state
            .cache_refreshers()
            .map(|(ecosystem, _)| ecosystem.clone())
            .collect::<Vec<_>>(),
        vec![crate::ECOSYSTEM]
    );
}

#[test]
fn serving_delegates_name_policy_cache_and_trash() {
    let (_dir, state) = state();
    let serving = PypiServing;

    assert_eq!(serving.normalize_name("Demo_Pkg"), "demo-pkg");
    assert!(serving.compile_policy(&toml::Table::new()).unwrap().is_empty());
    let known = toml::Table::from_iter([(
        "fallback_mode".to_owned(),
        toml::Value::String("no-fallback".to_owned()),
    )]);
    assert!(!serving.compile_policy(&known).unwrap().is_empty());
    let unknown = toml::Table::from_iter([("unknown".to_owned(), toml::Value::Boolean(true))]);
    assert_eq!(
        serving.compile_policy(&unknown).unwrap_err(),
        "unknown field `unknown` in `[index.policy]`"
    );
    assert!(
        serving
            .cache_pages(&state.serving.meta, &["hosted"])
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        serving.cache_record_counts(&state.serving.meta).unwrap(),
        [
            "file_url_records",
            "metadata_records",
            "publication_records",
            "project_records",
            "upload_records",
            "override_records",
            "provenance_records",
            "summary_count_records",
            "summary_order_records",
        ]
        .map(|kind| (kind.to_owned(), 0))
    );
    assert!(
        serving
            .trash_records(&state.serving.meta, &["hosted".to_owned()])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn serving_recognizes_token_basic_credentials() {
    let serving = PypiServing;
    for (authorization, expected) in [
        (format!("Basic {}", STANDARD.encode("__token__:secret")), true),
        (format!("Basic {}", STANDARD.encode("publisher:secret")), false),
        ("Bearer secret".to_owned(), false),
    ] {
        assert_eq!(serving.recognizes(&authorization), expected, "{authorization}");
    }
}

#[test]
fn serving_ignores_foreign_scheduled_jobs() {
    let settings = toml::Table::new();
    assert!(
        PypiServing
            .compile_job(JobConfig {
                kind: "foreign",
                settings: &settings,
                indexes: &[],
            })
            .is_none()
    );
}

#[test]
fn serving_rebuilds_nested_virtual_search_views() {
    let (_directory, state) = state_with_indexes(vec![
        hosted_index(),
        virtual_index("middle", vec![0]),
        virtual_index("outer", vec![1]),
    ]);
    seed_archive(&state.serving);

    PypiServing
        .apply_replicated_changes(&state.serving, &["pypi\0p\0hosted/demo".to_owned()])
        .unwrap();

    assert_eq!(
        state
            .serving
            .search
            .search(
                &state.search_ctx(),
                SearchParams {
                    query: "demo".to_owned(),
                    ..SearchParams::default()
                },
            )
            .unwrap()
            .total,
        3
    );
}

#[test]
fn serving_retires_nested_virtual_cached_renders() {
    let (_directory, state) = state_with_indexes(vec![
        hosted_index(),
        virtual_index("middle", vec![0]),
        virtual_index("outer", vec![1]),
    ]);
    let keys = ["hosted", "middle", "outer"].map(|route| {
        state
            .serving
            .representation_key(route, "demo", crate::cache::SIMPLE_HTML)
    });

    PypiServing
        .apply_replicated_changes(&state.serving, &["pypi\0p\0hosted/demo".to_owned()])
        .unwrap();

    for (route, key) in ["hosted", "middle", "outer"].into_iter().zip(keys) {
        assert_ne!(
            state
                .serving
                .representation_key(route, "demo", crate::cache::SIMPLE_HTML),
            key
        );
    }
}

#[tokio::test]
async fn serving_browses_and_inspects_a_hosted_archive() {
    let (_dir, state) = state();
    let (filename, digest) = seed_archive(&state.serving);
    let serving = PypiServing;
    let access = peryx_driver::access::ReadAccess::from_headers(&state.serving, &axum::http::HeaderMap::new());

    assert_project_browse(&serving, &state.serving).await;

    let archive_query = format!("index=hosted&project=demo&file={digest}%2F{filename}");
    let archive = serving
        .browse(BrowseRequest {
            state: state.serving.clone(),
            position: 0,
            raw_query: archive_query.clone(),
            access: &access,
            base: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(archive.title, filename);
    assert!(
        matches!(
            archive.sections.as_slice(),
            [BrowseSection::Table {
                heading,
                columns,
                rows,
                empty,
            }] if heading == "Archive members"
                && columns == &["Path", "Size", "Kind"]
                && rows.len() == 3
                && rows[0].cells[0].text == "README.txt"
                && rows[0].cells[1].text == (crate::archive::DEFAULT_MEMBER_CHUNK + 1).to_string()
                && rows[0].cells[2].text == "text"
                && rows[1].cells[0].text == "data.bin"
                && rows[1].cells[0].href.is_none()
                && rows[2].cells[0].text == "inner.zip"
                && rows[2].cells[0].href.as_deref()
                    == Some(format!("/browse?index=hosted&project=demo&sha256={digest}&file={filename}&container=inner.zip").as_str())
                && empty == "The archive has no members."
        ),
        "{:#?}",
        archive.sections
    );

    let member = serving
        .browse(BrowseRequest {
            state: state.serving.clone(),
            position: 0,
            raw_query: format!("{archive_query}&member=README.txt"),
            access: &access,
            base: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(member.title, "README.txt");
    assert!(matches!(
        member.sections.as_slice(),
        [BrowseSection::Content {
            heading,
            text,
            size: Some(size),
            offset: 0,
            next: Some(next),
        }] if heading == "Member"
            && text.len() == usize::try_from(crate::archive::DEFAULT_MEMBER_CHUNK).unwrap()
            && text.bytes().all(|byte| byte == b'a')
            && *size == crate::archive::DEFAULT_MEMBER_CHUNK + 1
            && next.href
                == format!(
                    "/browse?index=hosted&project=demo&sha256={digest}&file={filename}&member=README.txt&offset={}",
                    crate::archive::DEFAULT_MEMBER_CHUNK
                )
    ));

    assert_nested_archive(&serving, &state.serving, &archive_query, &filename, &digest).await;
}

async fn assert_project_browse(serving: &PypiServing, state: &Arc<ServingState>) {
    let access = peryx_driver::access::ReadAccess::from_headers(state, &axum::http::HeaderMap::new());
    let projects = serving
        .browse(BrowseRequest {
            state: state.clone(),
            position: 0,
            raw_query: "index=hosted".to_owned(),
            access: &access,
            base: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(projects.title, "hosted");
    assert!(
        matches!(
            projects.sections.as_slice(),
            [BrowseSection::Links { heading, entries, .. }]
                if heading == "Projects"
                    && entries.len() == 1
                    && entries[0].label == "Demo"
                    && entries[0].href == "/browse?index=hosted&project=Demo"
        ),
        "{:#?}",
        projects.sections
    );

    let project = serving
        .browse(BrowseRequest {
            state: state.clone(),
            position: 0,
            raw_query: "index=hosted&project=demo".to_owned(),
            access: &access,
            base: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(project.title, "Demo");
    assert!(
        project
            .sections
            .iter()
            .any(|section| matches!(section, BrowseSection::Table { heading, .. } if heading == "Files"))
    );
}

async fn assert_nested_archive(
    serving: &PypiServing,
    state: &Arc<ServingState>,
    archive_query: &str,
    filename: &str,
    digest: &str,
) {
    let access = peryx_driver::access::ReadAccess::from_headers(state, &axum::http::HeaderMap::new());
    let nested_query = format!("{archive_query}&container=inner.zip");
    let nested = serving
        .browse(BrowseRequest {
            state: state.clone(),
            position: 0,
            raw_query: nested_query.clone(),
            access: &access,
            base: None,
        })
        .await
        .unwrap()
        .unwrap();
    let nested_url = format!("/browse?index=hosted&project=demo&sha256={digest}&file={filename}&container=inner.zip");
    assert_eq!(
        (
            nested.title.as_str(),
            nested
                .breadcrumbs
                .last()
                .map(|link| (link.label.as_str(), link.href.as_str())),
        ),
        ("inner.zip", Some(("inner.zip", nested_url.as_str())))
    );
    assert_eq!(
        nested.sections,
        vec![BrowseSection::Table {
            heading: "Archive members".to_owned(),
            columns: vec!["Path".to_owned(), "Size".to_owned(), "Kind".to_owned()],
            rows: vec![peryx_core::BrowseRow {
                cells: vec![
                    peryx_core::BrowseCell {
                        text: "pkg/module.py".to_owned(),
                        href: Some(format!(
                            "/browse?index=hosted&project=demo&sha256={digest}&file={filename}&container=inner.zip&member=pkg%2Fmodule.py"
                        )),
                        code: true,
                    },
                    peryx_core::BrowseCell {
                        text: "12".to_owned(),
                        href: None,
                        code: false,
                    },
                    peryx_core::BrowseCell {
                        text: "text".to_owned(),
                        href: None,
                        code: false,
                    },
                ],
                badges: Vec::new(),
                actions: Vec::new(),
            }],
            empty: "The archive has no members.".to_owned(),
        }]
    );

    let nested_member = serving
        .browse(BrowseRequest {
            state: state.clone(),
            position: 0,
            raw_query: format!("{nested_query}&member=pkg%2Fmodule.py"),
            access: &access,
            base: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        nested_member.sections.as_slice(),
        [BrowseSection::Content { text, .. }] if text == "answer = 42\n"
    ));
}

fn state() -> (tempfile::TempDir, Arc<AppState>) {
    state_with_indexes(vec![hosted_index()])
}

fn state_with_indexes(indexes: Vec<Index>) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        indexes,
    );
    crate::tests::install(&mut state);
    (directory, Arc::new(state))
}

fn hosted_index() -> Index {
    Index {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

fn virtual_index(name: &str, layers: Vec<usize>) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Virtual {
            layers,
            write_target: None,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

fn seed_archive(state: &ServingState) -> (String, String) {
    let filename = "demo-1.0-py3-none-any.whl".to_owned();
    let mut inner = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut inner));
        archive
            .start_file("pkg/module.py", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"answer = 42\n").unwrap();
        archive.finish().unwrap();
    }
    let mut bytes = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        archive
            .start_file("README.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(&vec![
                b'a';
                usize::try_from(crate::archive::DEFAULT_MEMBER_CHUNK + 1).unwrap()
            ])
            .unwrap();
        archive
            .start_file("data.bin", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(&[0xff, 0xfe]).unwrap();
        archive
            .start_file("inner.zip", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(&inner).unwrap();
        archive.finish().unwrap();
    }
    let digest = Digest::of(&bytes);
    state.blobs.blocking().put_bytes_as(&bytes, &digest).unwrap();
    state
        .meta
        .put_upload(
            "hosted",
            "demo",
            &filename,
            crate::to_json(&Uploaded {
                version: "1.0".to_owned(),
                file: File {
                    filename: filename.clone(),
                    url: local_artifact_url("hosted", digest.as_str(), &filename),
                    hashes: BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
                    requires_python: None,
                    size: Some(bytes.len() as u64),
                    upload_time: None,
                    yanked: Yanked::No,
                    core_metadata: CoreMetadata::Absent,
                    dist_info_metadata: CoreMetadata::Absent,
                    gpg_sig: None,
                    provenance: Provenance::Absent,
                },
                trashed: None,
            })
            .as_bytes(),
        )
        .unwrap();
    state.meta.put_project("hosted", "demo", "Demo").unwrap();
    (filename, digest.as_str().to_owned())
}

#[test]
fn serving_advances_only_the_replicated_repositories_generation() {
    let (_directory, state) = state_with_indexes(vec![
        hosted_index(),
        Index {
            name: "other".to_owned(),
            route: "other".to_owned(),
            ..hosted_index()
        },
    ]);

    PypiServing
        .apply_replicated_changes(&state.serving, &["pypi\0u\0hosted/demo/demo-1.0.whl".to_owned()])
        .unwrap();

    assert_eq!(
        ["hosted", "other"].map(|index| state.serving.meta.policy_input_generation(index).unwrap()),
        [
            peryx_storage::meta::PolicyInputGeneration {
                repository: 1,
                catalog: 0,
                policy: 0,
            },
            peryx_storage::meta::PolicyInputGeneration::default(),
        ]
    );
}

#[test]
fn serving_holds_the_frontier_when_a_replicated_generation_cannot_advance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    drop(MetaStore::open(&path).unwrap());
    let database = redb::Database::open(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, &[u8]>::new("policy_input_generation"))
        .unwrap()
        .insert("hosted", b"{".as_slice())
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let mut state = AppState::new(
        MetaStore::open_existing(&path).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        vec![hosted_index()],
    );
    crate::tests::install(&mut state);

    let block = PypiServing
        .apply_replicated_changes(&state.serving, &["pypi\0u\0hosted/demo/demo-1.0.whl".to_owned()])
        .unwrap_err();

    assert_eq!(block.view, peryx_driver::state::SEARCH_VIEW);
}

/// The repair capability reaches the same audit `fsck` reports through, so a preview names the row and
/// a confirmed run rebuilds it.
#[test]
fn serving_previews_then_rebuilds_a_summary_row_no_write_path_maintained() {
    let (_dir, state) = state();
    state
        .serving
        .meta
        .put_driver_value("pypi\u{0}p\u{0}hosted/flask", b"Flask")
        .unwrap();
    let indexes = [hosted_index()];
    let mut previewed = Vec::new();
    let mut repaired = Vec::new();
    let mut again = Vec::new();

    let planned = PypiServing
        .preview_metadata_repair(&state.serving.meta, &indexes, &mut previewed)
        .unwrap();
    let rebuilt = PypiServing
        .repair_metadata(&state.serving.meta, &indexes, &mut repaired)
        .unwrap();
    let remaining = PypiServing
        .preview_metadata_repair(&state.serving.meta, &indexes, &mut again)
        .unwrap();

    assert_eq!((planned, rebuilt, remaining), (1, 1, 0));
    assert_eq!(previewed, repaired);
    assert!(again.is_empty());
}
