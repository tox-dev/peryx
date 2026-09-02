use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use peryx_core::path::local_artifact_url;
use peryx_core::{BrowseCell, BrowseLink, BrowsePage, BrowseRow, BrowseSection};
use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{
    EcosystemAuth as _, EcosystemBrowse, EcosystemConfig as _, EcosystemOpenApi, EcosystemRegistration as _,
    EcosystemSnippet as _, PluginAuthConfig, PluginIndexConfig,
};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_plugin_registry::OperatorJobOptions;
use peryx_plugin_registry::PluginAuthRegistration;
use peryx_policy::{
    Policy, RetentionClass, RetentionConfig, RetentionDecision, RetentionOutcome, RetentionPolicy, RetentionSelector,
    RetentionVisibility,
};
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use utoipa::openapi::PathsBuilder;

use super::{
    CoreMetadata, DEFAULT_INDEXES, ECOSYSTEM, File, Provenance, PypiPlugin, Yanked, default_job_concurrency,
    default_job_project_limit, default_job_timeout_secs, registration,
};
use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::upload::Uploaded;

#[test]
fn plugin_exposes_capabilities_and_validates_snippet_formats() {
    let plugin = PypiPlugin;
    let base = BaseUrl::parse("https://packages.example/").unwrap();

    let registry = peryx_plugin_registry::PluginRegistry::new(vec![registration()])
        .unwrap()
        .activate([ECOSYSTEM])
        .unwrap();
    assert!(registry.drivers().get_job(&ECOSYSTEM).is_some());
    assert!(registry.drivers().get_policy(&ECOSYSTEM).is_some());
    assert!(registry.drivers().get_policy_dry_run(&ECOSYSTEM).is_some());
    assert!(registry.drivers().get_retention(&ECOSYSTEM).is_some());
    assert!(registry.drivers().get_cache_purge(&ECOSYSTEM).is_some());
    assert_eq!(
        registry
            .drivers()
            .cache_inspect_drivers()
            .map(|(ecosystem, _)| ecosystem.clone())
            .collect::<Vec<_>>(),
        vec![ECOSYSTEM]
    );
    assert!(plugin.text(&base, "pypi", true, "unknown").is_err());
}

#[rstest]
#[case::pip("pip.conf", "[global]")]
#[case::uv("uv.toml", "[[index]]")]
#[case::upload(".pypirc", "[distutils]")]
fn plugin_renders_each_snippet_format(#[case] format: &str, #[case] marker: &str) {
    let text = PypiPlugin
        .text(
            &BaseUrl::parse("https://packages.example/").unwrap(),
            "pypi",
            true,
            format,
        )
        .unwrap()
        .unwrap();

    assert!(text.contains(marker), "{text}");
}

#[test]
fn plugin_exposes_identity_defaults_and_driver() {
    let plugin = PypiPlugin;

    assert_eq!(plugin.ecosystem(), ECOSYSTEM);
    assert_eq!(plugin.default_indexes(), DEFAULT_INDEXES);
    assert_eq!(
        plugin.webhook_events(),
        ["delete", "restore", "unyank", "upload", "yank"]
    );
    assert_eq!(plugin.driver().ecosystem(), ECOSYSTEM);
    assert!(plugin.driver().indexed().is_some());
    let registration = registration();
    assert!(registration.auth.is_some());
    assert!(registration.browse.is_some());
    assert!(registration.snippets.is_some());
    assert_eq!(
        (
            registration.distributed_runtime.is_some(),
            registration.rate_limit_principal.is_some(),
            registration.client_discovery.is_some(),
        ),
        (true, true, true),
    );
}

#[test]
fn plugin_exposes_trusted_publishing_auth_configuration() {
    assert!(matches!(
        registration().auth,
        Some(PluginAuthRegistration::Extension { .. })
    ));
}

#[test]
fn plugin_auth_validation_requires_a_signing_key_for_publishers() {
    let values = trusted_publisher_values();
    let indexes = [PluginIndexConfig {
        name: "hosted",
        ecosystem: ECOSYSTEM,
        writable: true,
    }];

    assert_eq!(
        PypiPlugin.validate(PluginAuthConfig {
            values: &values,
            signing_key_configured: false,
            token_ttl_secs: 300,
            indexes: &indexes,
        }),
        Err("auth: `signing_key` is required when trusted publishers are configured".to_owned())
    );
}

#[test]
fn plugin_auth_installation_requires_the_validated_signing_key() {
    let (_temp_dir, mut state) = state();

    assert_eq!(
        peryx_driver::serving::EcosystemAuth::install(
            &PypiPlugin,
            &mut state.auth_install_context().unwrap(),
            &trusted_publisher_values(),
        ),
        Err("auth: `signing_key` is required when trusted publishers are configured".to_owned())
    );
}

#[test]
fn plugin_owns_the_catalog_operator_job() {
    let job = registration().operator_jobs[0];

    assert_eq!(job.command(), "run");
    assert_eq!(
        job.defaults(),
        peryx_plugin_registry::OperatorJobDefaults {
            item_limit: default_job_project_limit(),
            concurrency: default_job_concurrency(),
            timeout_secs: default_job_timeout_secs(),
        }
    );
    let scheduled = job
        .compile(OperatorJobOptions {
            target: "packages",
            source: None,
            item_limit: default_job_project_limit(),
            concurrency: default_job_concurrency(),
            timeout_secs: default_job_timeout_secs(),
        })
        .unwrap();
    assert_eq!((scheduled.ecosystem(), scheduled.kind()), (ECOSYSTEM, "catalog_sync"));
}

#[test]
fn plugin_compiles_empty_settings_and_rejects_unknown_fields() {
    let plugin = PypiPlugin;

    assert!(
        plugin
            .compile_index_settings("pypi", &toml::Table::new())
            .unwrap()
            .is_none()
    );
    let settings = toml::Table::from_iter([("unknown".to_owned(), toml::Value::Boolean(true))]);
    assert_eq!(
        plugin.compile_index_settings("pypi", &settings).unwrap_err(),
        "compile settings for pypi: unknown field `unknown` in `[index.settings]`"
    );
}

#[test]
fn plugin_install_registers_the_pypi_runtime() {
    let (_temp_dir, mut state) = state();

    plugin_install(&mut state);

    assert!(state.indexed_driver_for(&ECOSYSTEM).is_some());
    assert!(state.driver_set().get_job(&ECOSYSTEM).is_some());
    assert!(state.mirror_driver_for(&ECOSYSTEM).is_some());
}

#[test]
fn plugin_online_cache_inspection_matches_offline_reads() {
    let (_temp_dir, mut state) = hosted_state();
    state
        .serving
        .meta
        .put_index(
            "hosted/flask",
            &CachedIndex {
                source: None,
                last_modified: None,
                etag: Some("v1".to_owned()),
                last_serial: Some(1),
                fetched_at_unix: 100,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: Some(60),
                body: br#"{"files":[]}"#.to_vec(),
            },
        )
        .unwrap();
    plugin_install(&mut state);
    let offline = state.driver_set().get_cache(&ECOSYSTEM).unwrap();
    let (_, online) = state.driver_set().cache_inspect_drivers().next().unwrap();

    let offline_pages = offline.cache_pages(&state.serving.meta, &["hosted"]).unwrap();
    let online_pages = online.served_cache_pages(&state.serving, &["hosted"]).unwrap();

    assert_eq!(online_pages, offline_pages);
    assert_eq!(
        online_pages
            .iter()
            .map(|page| page.resource.as_str())
            .collect::<Vec<_>>(),
        ["flask"]
    );
    assert_eq!(
        online.served_cache_record_counts(&state.serving).unwrap(),
        offline.cache_record_counts(&state.serving.meta).unwrap()
    );
}

fn plugin_install(state: &mut peryx_driver::AppState) {
    let plugins = peryx_plugin_registry::PluginRegistry::new(vec![crate::registration()])
        .unwrap()
        .activate([ECOSYSTEM])
        .unwrap();
    plugins.register_activated_capabilities(&mut state.capability_install_context());
    plugins
        .install_drivers(
            &mut state.runtime_install_context().unwrap(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
}

#[test]
fn plugin_local_installation_disables_replication() {
    let (_temp_dir, mut state) = state();

    plugin_install(&mut state);

    assert!(!crate::replication_enabled(&state.serving));
    assert_eq!(state.replicated_apply_drivers().count(), 0);
}

#[test]
fn plugin_distributed_installation_registers_the_pypi_runtime() {
    let (_temp_dir, mut state) = state();

    peryx_driver::serving::DistributedRuntime::install(
        &PypiPlugin,
        &mut state.distributed_install_context().unwrap(),
        &[],
    )
    .unwrap();

    assert!(state.indexed_driver_for(&ECOSYSTEM).is_some());
    assert!(crate::replication_enabled(&state.serving));
    assert_eq!(state.replicated_apply_drivers().count(), 1);
    assert!(state.mirror_driver_for(&ECOSYSTEM).is_some());
    assert_eq!(state.http_routes().count(), 1);
}

#[test]
fn plugin_exposes_every_browse_path() {
    assert_eq!(
        EcosystemBrowse::paths(&PypiPlugin),
        &[
            "/upload",
            "/+ui/projects",
            "/+ui/project",
            "/+ui/members",
            "/+ui/member",
            "/+ui/browse",
        ]
    );
}

#[tokio::test]
async fn plugin_browse_dispatch_reports_a_missing_index_query() {
    let (_temp_dir, state) = state();
    let response = EcosystemBrowse::dispatch(
        &PypiPlugin,
        Arc::new(state),
        Request::builder().uri("/+ui/projects").body(Body::empty()).unwrap(),
    )
    .await;
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();

    assert_eq!(
        (status, body.as_ref()),
        (StatusCode::BAD_REQUEST, b"missing index".as_slice())
    );
}

#[tokio::test]
async fn plugin_browse_dispatch_ignores_unknown_fields_and_links_archive_members() {
    let (_temp_dir, state) = hosted_state();
    let filename = "demo-1.0-py3-none-any.whl";
    let mut bytes = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        archive
            .start_file("README.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"read me\n").unwrap();
        archive.finish().unwrap();
    }
    let digest = Digest::of(&bytes);
    let digest_hex = digest.as_str();
    state.serving.blobs.blocking().put_bytes_as(&bytes, &digest).unwrap();
    state
        .serving
        .meta
        .put_upload(
            "hosted",
            "demo",
            filename,
            &serde_json::to_vec(&uploaded(filename, "1.0", digest_hex, bytes.len() as u64)).unwrap(),
        )
        .unwrap();
    state.serving.meta.put_project("hosted", "demo", "Demo").unwrap();

    let response = EcosystemBrowse::dispatch(
        &PypiPlugin,
        Arc::new(state),
        Request::builder()
            .uri(format!(
                "/+ui/members?index=hosted&project=demo&sha256={digest_hex}&file={filename}&unknown=ignored"
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let status = response.status();
    let page: BrowsePage = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert_eq!(
        (status, page),
        (
            StatusCode::OK,
            BrowsePage {
                breadcrumbs: vec![
                    BrowseLink {
                        label: "hosted".to_owned(),
                        href: "/browse?index=hosted".to_owned(),
                    },
                    BrowseLink {
                        label: "demo".to_owned(),
                        href: "/browse?index=hosted&project=demo".to_owned(),
                    },
                    BrowseLink {
                        label: filename.to_owned(),
                        href: format!("/browse?index=hosted&project=demo&sha256={digest_hex}&file={filename}"),
                    },
                ],
                title: filename.to_owned(),
                sections: vec![BrowseSection::Table {
                    heading: "Archive members".to_owned(),
                    columns: vec!["Path".to_owned(), "Size".to_owned(), "Kind".to_owned()],
                    rows: vec![BrowseRow {
                        cells: vec![
                            BrowseCell {
                                text: "README.txt".to_owned(),
                                href: Some(format!(
                                    "/browse?index=hosted&project=demo&sha256={digest_hex}&file={filename}&member=README.txt"
                                )),
                                code: true,
                            },
                            BrowseCell {
                                text: "8".to_owned(),
                                href: None,
                                code: false,
                            },
                            BrowseCell {
                                text: "text".to_owned(),
                                href: None,
                                code: false,
                            },
                        ],
                        badges: Vec::new(),
                        actions: Vec::new(),
                    }],
                    empty: "The archive has no members.".to_owned(),
                }],
                ..BrowsePage::default()
            },
        )
    );
}

fn version_digest(version: &str) -> String {
    Digest::of(version.as_bytes()).as_str().to_owned()
}

#[test]
fn plugin_retention_capability_plans_hosted_uploads() {
    let (_temp_dir, state) = state();
    for version in ["2.0", "1.0"] {
        let filename = format!("demo-{version}.whl");
        state
            .serving
            .meta
            .put_upload(
                "hosted",
                "demo",
                &filename,
                &serde_json::to_vec(&uploaded(&filename, version, &version_digest(version), 1)).unwrap(),
            )
            .unwrap();
    }
    let policy = RetentionPolicy::compile(
        &RetentionConfig {
            keep: vec![RetentionSelector::KeepLatestGroups { count: 1 }],
            expire: vec![RetentionSelector::ResourcePrefix { prefix: String::new() }],
        },
        crate::normalize_name,
    );
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![registration()])
        .unwrap()
        .activate([ECOSYSTEM])
        .unwrap();
    let mut decisions = Vec::new();
    let mut summary = None;
    registry
        .drivers()
        .get_retention(&ECOSYSTEM)
        .unwrap()
        .plan_retention(
            &peryx_driver::serving::RetentionScan {
                meta: &state.serving.meta,
                index: "hosted",
                policy: &policy,
                now: None,
                cancellation: &peryx_driver::ScanCancellation::new(),
            },
            &mut |current| {
                summary = Some(current);
                Ok(())
            },
            &mut |decision| {
                decisions.push(decision);
                Ok(())
            },
        )
        .unwrap();
    let summary = summary.unwrap();

    assert_eq!(
        (summary.policy_version, decisions),
        (
            policy.version(),
            vec![
                RetentionDecision {
                    resource: "demo".to_owned(),
                    group: Some("2.0".to_owned()),
                    artifact: "demo-2.0.whl".to_owned(),
                    digest: version_digest("2.0"),
                    class: RetentionClass::Hosted,
                    visibility: RetentionVisibility::Active,
                    source: None,
                    bytes: 1,
                    outcome: RetentionOutcome::Retain,
                    rule: Some("keep-latest-groups"),
                    retained_groups: Vec::new(),
                },
                RetentionDecision {
                    resource: "demo".to_owned(),
                    group: Some("1.0".to_owned()),
                    artifact: "demo-1.0.whl".to_owned(),
                    digest: version_digest("1.0"),
                    class: RetentionClass::Hosted,
                    visibility: RetentionVisibility::Active,
                    source: None,
                    bytes: 1,
                    outcome: RetentionOutcome::Remove,
                    rule: Some("resource-prefix"),
                    retained_groups: vec!["2.0".to_owned()],
                },
            ],
        )
    );
}

#[test]
fn plugin_openapi_contains_every_pypi_surface() {
    let paths = serde_json::to_value(EcosystemOpenApi::paths(&PypiPlugin, PathsBuilder::new()).build()).unwrap();
    let actual: BTreeSet<&str> = paths.as_object().unwrap().keys().map(String::as_str).collect();
    let expected = BTreeSet::from([
        "/_/oidc/audience",
        "/_/oidc/mint-token",
        "/+shadow/candidates",
        "/{route}/",
        "/{route}/+api",
        "/{route}/+search",
        "/{route}/files/{sha256}/{filename}",
        "/{route}/files/{sha256}/{filename}.metadata",
        "/{route}/inspect/{sha256}/{filename}",
        "/{route}/inspect/{sha256}/{filename}/{member}",
        "/{route}/simple/",
        "/{route}/simple/{project}/",
        "/{route}/{project}/",
        "/{route}/{project}/json",
        "/{route}/{project}/{version}/",
        "/{route}/{project}/{version}/json",
        "/{route}/{project}/{version}/promote",
        "/{route}/{project}/{version}/restore",
        "/{route}/{project}/{version}/yank",
    ]);

    assert_eq!(actual, expected);
}

fn trusted_publisher_values() -> toml::Table {
    toml::from_str(
        r#"
[[trusted_publisher]]
id = "release"
issuer = "https://issuer.example"
repository = "hosted"
subject = "repo:org/app:*"
projects = ["app"]
"#,
    )
    .unwrap()
}

fn state() -> (tempfile::TempDir, peryx_driver::AppState) {
    state_with_indexes(Vec::new())
}

fn hosted_state() -> (tempfile::TempDir, peryx_driver::AppState) {
    state_with_indexes(vec![Index {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        ecosystem: ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }])
}

fn state_with_indexes(indexes: Vec<Index>) -> (tempfile::TempDir, peryx_driver::AppState) {
    let temp_dir = tempfile::tempdir().unwrap();
    let state = peryx_driver::AppState::new(
        MetaStore::open(temp_dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(temp_dir.path().join("blobs")),
        60,
        indexes,
    );
    (temp_dir, state)
}

fn uploaded(filename: &str, version: &str, digest: &str, size: u64) -> Uploaded {
    Uploaded {
        version: version.to_owned(),
        file: File {
            filename: filename.to_owned(),
            url: local_artifact_url("hosted", digest, filename),
            hashes: BTreeMap::from([("sha256".to_owned(), digest.to_owned())]),
            requires_python: None,
            size: Some(size),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: None,
    }
}
