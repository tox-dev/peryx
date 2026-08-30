use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use peryx_identity::{Action, ProviderId};
use peryx_policy::PolicyAction;
use peryx_storage::blob::Digest;
use peryx_storage::meta::{
    AccountingClass, JobKind, JobState, MetaStore, NewJobRun, NewQuotaReservation, PolicyDecisionQuery, QuotaLimits,
};
use peryx_upstream::Auth;
use rstest::rstest;
use tower::ServiceExt as _;

use crate::config::{
    AuthConfig, AvailabilityConfig, BlobStorageConfig, Config, DcMember, DcMembership, DcRole, LdapBindConfig,
    LdapProviderConfig, OidcProviderConfig, ReplicationConfig, S3StorageConfig, SecretSource, WebhookConfig,
    WebhookSecret,
};
use crate::server::{
    build_indexes_with_plugins, build_router_with_plugins, build_state_with_plugins, check_config_with_plugins,
    recover_job_attempts,
};
use crate::tests::support::{plugins, plugins_with_inactive_owner, plugins_without_retention};

const TOKEN_REALM_SIGNING_KEY: &str = "test-token-realm-signing-key-32-bytes";

fn s3_blob_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        blob: BlobStorageConfig::S3(S3StorageConfig {
            endpoint: "https://s3.example.com".to_owned(),
            bucket: "cache".to_owned(),
            prefix: String::new(),
            region: "us-east-1".to_owned(),
            path_style: true,
            request_timeout: Duration::from_secs(30),
            max_retries: 3,
            multipart_threshold: 16 << 20,
            part_size: 16 << 20,
            upload_concurrency: 4,
            conditional_writes: true,
            checksum_writes: true,
        }),
        ..neutral_config()
    }
}

#[test]
fn test_build_blob_storage_selects_the_filesystem_backend() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral_config()
    };
    assert_eq!(build_state(&config).unwrap().serving.blobs.name(), "filesystem");
}

#[test]
fn test_build_blob_storage_opens_the_s3_backend() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(build_state(&s3_blob_config(&dir)).unwrap().serving.blobs.name(), "s3");
}

#[test]
fn test_plugin_registry_rejects_settings_for_an_uninstalled_ecosystem() {
    let ecosystem = peryx_core::Ecosystem::new("missing");

    let error = crate::compiled_plugins()
        .compile_index_settings(&ecosystem, "index", &toml::Table::new())
        .unwrap_err();

    assert_eq!(error, "ecosystem missing is not installed");
}

#[test]
fn test_plugin_registry_rejects_snippets_for_an_uninstalled_ecosystem() {
    let ecosystem = peryx_core::Ecosystem::new("missing");
    let base = peryx_driver::discovery::BaseUrl::parse("https://artifacts.example/").unwrap();

    let error = crate::compiled_plugins()
        .snippet_text(&ecosystem, &base, "index", false, "text")
        .unwrap_err();

    assert_eq!(error, "ecosystem missing is not installed");
}

#[test]
fn configured_indexes_select_owner_runtime_and_openapi() {
    let plugins = crate::compiled_plugins();
    let ecosystems = plugins
        .default_indexes()
        .map(|index| index.ecosystem.clone())
        .collect::<std::collections::HashSet<_>>();

    for ecosystem in &ecosystems {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::with_plugins(&plugins);
        config.data_dir = directory.path().to_path_buf();
        config.indexes.retain(|index| &index.ecosystem == ecosystem);
        let state = build_state_with_plugins(&config, &plugins).unwrap();
        let active = plugins.activate([ecosystem.clone()]).unwrap();

        assert_eq!(
            ecosystems
                .iter()
                .map(|candidate| (candidate.clone(), state.driver_for(candidate).is_some()))
                .collect::<std::collections::HashMap<_, _>>(),
            ecosystems
                .iter()
                .map(|candidate| (candidate.clone(), candidate == ecosystem))
                .collect()
        );
        assert_eq!(
            state.openapi(),
            crate::api::openapi_json_for_with_plugins(peryx_ha::AvailabilityResources::None, &active)
        );
    }
}

#[test]
fn empty_index_configuration_installs_no_owner_runtime() {
    let plugins = crate::compiled_plugins();
    let ecosystems = plugins
        .default_indexes()
        .map(|index| index.ecosystem.clone())
        .collect::<std::collections::HashSet<_>>();
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::with_plugins(&plugins);
    config.data_dir = directory.path().to_path_buf();
    config.indexes.clear();
    let state = build_state_with_plugins(&config, &plugins).unwrap();

    assert!(ecosystems.iter().all(|ecosystem| state.driver_for(ecosystem).is_none()));
    assert_eq!(state.http_routes().count(), 0);
    assert_eq!(
        state.openapi(),
        crate::api::openapi_json_for_with_plugins(
            peryx_ha::AvailabilityResources::None,
            &plugins.activate([]).unwrap(),
        )
    );
}

#[test]
fn inactive_owner_migrations_and_ha_references_do_not_run() {
    let plugins = plugins_with_inactive_owner(Some(Arc::new(peryx_ecosystem_pypi::PypiPlugin)));
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: directory.path().to_path_buf(),
        ..Config::with_plugins(&plugins)
    };
    let state = build_state_with_plugins(&config, &plugins).unwrap();
    let active = plugins
        .activate(config.indexes.iter().map(|index| index.ecosystem.clone()))
        .unwrap();
    assert!(!active.has_metadata_migrations());
    let references = peryx_ha_distributed::reference_inventory(
        active.drivers().clone(),
        state.serving.meta.clone(),
        config.indexes.iter().map(|index| index.name.clone()).collect(),
    );

    assert_eq!(
        references.referenced().unwrap(),
        [Digest::of(b"artifact bytes"), Digest::of(b"metadata bytes")]
            .into_iter()
            .map(|digest| digest.as_str().to_owned())
            .collect()
    );
}

#[test]
fn test_build_indexes_preserves_names_without_a_name_capability() {
    let plugins = plugins_without_retention();
    let mut config = Config::with_plugins(&plugins);
    config.indexes[0].policy.block_resources = vec!["Mixed.Name".to_owned()];

    let indexes = build_indexes_with_plugins(&config.indexes, &config.auth, false, &plugins).unwrap();

    assert!(
        indexes[0]
            .policy
            .check_resource(PolicyAction::Upload, "Mixed.Name")
            .is_err()
    );
    assert!(
        indexes[0]
            .policy
            .check_resource(PolicyAction::Upload, "mixed.name")
            .is_ok()
    );
}

#[test]
fn test_build_state_preserves_upstream_names_without_a_name_capability() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = plugins_without_retention();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::with_plugins(&plugins)
    }
    .apply_with_plugins(
        crate::config::from_toml(
            dir.path().join("peryx.toml"),
            r#"
[[index]]
ecosystem = "plain"
name = "cache"
protected = ["Mixed.Name"]

[[index.upstream]]
name = "primary"
url = "https://primary.example/"

[[index.upstream]]
name = "secondary"
url = "https://secondary.example/"
"#,
        )
        .unwrap(),
        &plugins,
    )
    .unwrap();

    let state = build_state_with_plugins(&config, &plugins).unwrap();
    let route = &state.serving.upstream_routes["cache"];

    assert_eq!(
        route
            .candidates("Mixed.Name")
            .map(peryx_upstream::NamedUpstream::name)
            .collect::<Vec<_>>(),
        ["primary"]
    );
    assert_eq!(
        route
            .candidates("mixed.name")
            .map(peryx_upstream::NamedUpstream::name)
            .collect::<Vec<_>>(),
        ["primary", "secondary"]
    );
}

#[test]
fn test_build_indexes_rejects_policy_without_a_policy_capability() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = plugins_without_retention();
    let mut config = Config::with_plugins(&plugins);
    config.data_dir = dir.path().to_path_buf();
    config.indexes[0]
        .ecosystem_policy
        .insert("rule".to_owned(), toml::Value::Boolean(true));

    let error = build_state_with_plugins(&config, &plugins)
        .err()
        .expect("expected policy compilation error");

    assert_eq!(error.to_string(), "compile policy for plain");
}

fn claim_writer(dir: &tempfile::TempDir, identity: &str) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(identity)
        .unwrap();
}
fn availability_replica() -> AvailabilityConfig {
    availability_replica_with_poll(Duration::from_secs(1))
}

fn availability_replica_with_poll(poll_interval: Duration) -> AvailabilityConfig {
    AvailabilityConfig::Dc(ReplicationConfig::Replica {
        upstream: "https://writer.example/".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
        poll_interval,
        page_size: NonZeroUsize::MIN,
    })
}

fn availability_primary() -> AvailabilityConfig {
    AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "writer-a".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
    })
}
#[rstest]
#[case::liveness("/+health", r#"{"status":"live"}"#)]
#[case::readiness("/+ready", r#"{"status":"ready"}"#)]
fn test_build_router_serves_public_probes(#[case] uri: &str, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    tokio::task::LocalSet::new().block_on(
        &tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
        async {
            let router = build_router(&Config {
                data_dir: dir.path().to_path_buf(),
                ..neutral_config()
            })
            .unwrap();
            let response = router
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body.as_ref(), expected.as_bytes());
        },
    );
}

#[test]
#[cfg(all(feature = "composition-oci", feature = "composition-pypi"))]
fn test_shipped_plugins_compose_and_serve_their_protocols() {
    let plugins = peryx_plugin_registry::PluginRegistry::new(vec![
        peryx_ecosystem_pypi::registration(),
        peryx_ecosystem_oci::registration(),
    ])
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    tokio::task::LocalSet::new().block_on(
        &tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
        async {
            let router = build_router_with_plugins(
                &Config {
                    data_dir: dir.path().to_path_buf(),
                    ..Config::with_plugins(&plugins)
                },
                &plugins,
            )
            .unwrap();
            let mut responses = Vec::new();
            for uri in ["/hosted/simple/", "/v2/"] {
                responses.push(
                    router
                        .clone()
                        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                        .await
                        .unwrap()
                        .status(),
                );
            }
            assert_eq!(responses, [StatusCode::OK, StatusCode::OK]);
        },
    );
}

#[test]
fn test_build_router_installs_distributed_routes() {
    let dir = tempfile::tempdir().unwrap();
    tokio::task::LocalSet::new().block_on(
        &tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
        async {
            let router = build_router(&Config {
                data_dir: dir.path().to_path_buf(),
                availability: AvailabilityConfig::Dc(ReplicationConfig::Primary {
                    source: "primary-a".to_owned(),
                    token: SecretSource::Literal("replication-token".to_owned()),
                }),
                ..neutral_config()
            })
            .unwrap();
            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/+replication/v1/health")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
        },
    );
}
#[test]
fn test_build_state_opens_configured_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral_config()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(state.serving.indexes.len(), config.indexes.len());
    assert!(dir.path().join("peryx.redb").exists());
}

#[test]
fn test_build_state_reports_configured_repository_persistence_failure() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with_corrupt_repository(&dir);

    let error = build_state(&config)
        .err()
        .expect("expected repository persistence error");

    assert_eq!(
        error.to_string(),
        format!("persist configured repositories [{}]", config.indexes[0].route)
    );
}

#[test]
fn test_build_state_read_only_skips_repository_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_with_corrupt_repository(&dir);
    config.read_only = true;

    let state = build_state(&config).unwrap();

    assert_eq!(state.serving.indexes[0].route, config.indexes[0].route);
}

#[test]
fn test_build_state_preserves_configured_repository_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral_config()
    };
    let first = build_state(&config).unwrap();
    let first_repositories = routes_to_id_and_version(&first.serving.meta);
    drop(first);

    let second = build_state(&config).unwrap();

    assert_eq!(
        (routes_to_id_and_version(&second.serving.meta), first_repositories.len()),
        (first_repositories, config.indexes.len())
    );
}

fn config_with_corrupt_repository(dir: &tempfile::TempDir) -> Config {
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral_config()
    };
    config.indexes.truncate(1);
    let state = build_state(&config).unwrap();
    let repository = state
        .serving
        .meta
        .repository_by_route(&config.indexes[0].route)
        .unwrap()
        .unwrap();
    drop(state);
    let database = redb::Database::open(dir.path().join("peryx.redb")).unwrap();
    let transaction = database.begin_write().unwrap();
    {
        let mut repositories = transaction
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("repository"))
            .unwrap();
        repositories.insert(repository.id.as_str(), b"{".as_slice()).unwrap();
    }
    transaction.commit().unwrap();
    config
}

fn routes_to_id_and_version(store: &MetaStore) -> std::collections::BTreeMap<String, (String, u64)> {
    store
        .list_repositories(&peryx_storage::meta::RepositoryQuery {
            limit: 100,
            ..peryx_storage::meta::RepositoryQuery::default()
        })
        .unwrap()
        .repositories
        .into_iter()
        .map(|record| (record.route, (record.id.as_str().to_owned(), record.version)))
        .collect()
}

#[test]
fn test_build_state_repairs_abandoned_quota_before_admission() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral_config()
    };
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    for serial in 0..=peryx_driver::jobs::QUOTA_REPAIR_BATCH {
        let digest = format!("sha256:stale-{serial}");
        meta.reserve_quota(
            NewQuotaReservation {
                repository: "private",
                resource: Some(&digest),
                group: None,
                digest: &digest,
                bytes: 1,
                class: AccountingClass::Hosted,
                created_at_unix: 1,
            },
            QuotaLimits::default(),
        )
        .unwrap();
    }
    drop(meta);

    let state = build_state(&config).unwrap();
    let reservation = state
        .serving
        .meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "private",
                resource: Some("next"),
                group: None,
                digest: "sha256:next",
                bytes: 1,
                class: AccountingClass::Hosted,
                created_at_unix: 2,
            },
            QuotaLimits {
                max_accounted_bytes: Some(1),
                ..QuotaLimits::default()
            },
        )
        .unwrap();

    assert_eq!(reservation.bytes, 1);
}

#[rstest]
#[case::writer(false, 1, JobState::Failed)]
#[case::read_only(true, 0, JobState::Running)]
fn test_recover_job_attempts_only_updates_writers(
    #[case] read_only: bool,
    #[case] expected_recovered: usize,
    #[case] expected_state: JobState,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral_config()
    })
    .unwrap();
    let mut state = Arc::into_inner(state).expect("newly built state has no other owners");
    state.set_read_only(read_only).unwrap();
    let state = Arc::new(state);
    let id = state
        .serving
        .meta
        .start_job_run(NewJobRun {
            kind: JobKind::new("cache_refresh").unwrap(),
            scope: "fixture",
            repository: None,
            started_at_unix: 1,
        })
        .unwrap();

    assert_eq!(recover_job_attempts(&state).unwrap(), expected_recovered);
    assert_eq!(
        state.serving.meta.get_job_run(&id).unwrap().unwrap().state,
        expected_state
    );
}

#[test]
fn test_build_state_claims_configured_writer_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        availability: availability_primary(),
        ..neutral_config()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(
        state.serving.meta.writer_identity().unwrap().as_deref(),
        Some("writer-a")
    );
}

#[test]
fn test_build_state_rejects_a_competing_writer_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    store.claim_writer_identity("writer-a").unwrap();
    drop(store);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-b".to_owned()),
        availability: availability_primary(),
        ..neutral_config()
    };

    let error = build_state(&config).err().expect("expected writer identity conflict");

    let message = format!("{error:#}");
    assert!(message.contains("claim writer identity \"writer-b\""), "{message}");
    assert!(message.contains("claimed by writer \"writer-a\""), "{message}");
}

#[test]
fn test_build_state_rejects_a_replica_without_writer_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        availability: availability_replica(),
        ..neutral_config()
    };

    let error = build_state(&config)
        .err()
        .expect("expected invalid replica configuration");

    assert_eq!(
        format!("{error:#}"),
        "validate configuration: writer identity: required in read replica mode"
    );
    assert!(!dir.path().join("peryx.redb").exists());
}

#[test]
fn test_build_state_replica_does_not_claim_writer_identity() {
    let dir = tempfile::tempdir().unwrap();
    claim_writer(&dir, "writer-a");

    let state = build_state(&Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        availability: availability_replica(),
        ..neutral_config()
    })
    .unwrap();

    assert!(state.serving.read_only);
    assert_eq!(
        state.serving.meta.writer_identity().unwrap().as_deref(),
        Some("writer-a")
    );
}

#[test]
fn test_configured_replica_reports_its_poll_interval_on_rejected_mutations() {
    let dir = tempfile::tempdir().unwrap();
    claim_writer(&dir, "writer-a");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        availability: availability_replica_with_poll(Duration::from_secs(7)),
        ..neutral_config()
    };

    tokio::task::LocalSet::new().block_on(
        &tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
        async {
            let response = build_router(&config)
                .unwrap()
                .oneshot(
                    Request::builder()
                        .method(axum::http::Method::POST)
                        .uri("/+repositories")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()[header::RETRY_AFTER], "7");
        },
    );
}

#[test]
fn test_configured_replica_keeps_only_read_grants_and_disables_webhooks() {
    let dir = tempfile::tempdir().unwrap();
    claim_writer(&dir, "writer-a");
    let mut config = parsed_config(
        &dir,
        "[[index]]\nname = \"hosted\"\nhosted = true\n\
         [[index.access_token]]\nname = \"client\"\nsecret = \"secret\"\nactions = [\"read\", \"write\"]\n\
         [[index.webhook]]\nname = \"ci\"\nurl = \"https://hooks.example/ci\"\nsecret = \"hook-secret\"\n",
    );
    config.writer_identity = Some("writer-a".to_owned());
    config.availability = availability_replica();

    let state = build_state(&config).unwrap();

    assert!(state.serving.indexes[0].acl.grants_to_anyone(Action::Read));
    assert!(!state.serving.indexes[0].acl.grants_to_anyone(Action::Write));
    assert!(state.serving.webhooks.is_empty());
}

#[rstest]
#[case::missing(None, "None")]
#[case::different(Some("writer-b"), "Some(\"writer-b\")")]
fn test_build_state_rejects_a_replica_with_an_unmatched_writer(#[case] active: Option<&str>, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    if let Some(active) = active {
        claim_writer(&dir, active);
    }
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        availability: availability_replica(),
        ..neutral_config()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected replica writer identity conflict");
    };

    assert_eq!(
        error.to_string(),
        format!("configured replica writer Some(\"writer-a\") does not match metadata store writer {expected}")
    );
}

#[test]
fn test_build_state_reports_metadata_store_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("peryx.redb")).unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral_config()
    };

    let err = build_state(&config).err().expect("expected metadata store error");

    assert!(err.to_string().contains("open metadata store"));
}

#[test]
fn test_build_state_wires_the_token_realm_signing_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            signing_key: Some(SecretSource::Literal(TOKEN_REALM_SIGNING_KEY.to_owned())),
            token_ttl_secs: 900,
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let state = build_state(&config).unwrap();

    assert!(state.serving.signer.is_some());
    assert_eq!(state.serving.token_ttl_secs, 900);
}

#[test]
fn test_token_realm_boundaries_reject_a_short_signing_key() {
    for (boundary, source) in SIGNING_KEY_CASES {
        let dir = tempfile::tempdir().unwrap();
        let secret = "x".repeat(31);
        let config = signing_key_config(dir.path(), source(dir.path(), &secret));

        let error = boundary.validate(&config).unwrap_err().to_string();

        assert_eq!(error, "`auth.signing_key` must contain at least 32 bytes");
        assert!(!error.contains(&secret));
    }
}

#[test]
fn test_token_realm_boundaries_accept_a_32_byte_signing_key() {
    for (boundary, source) in SIGNING_KEY_CASES {
        let dir = tempfile::tempdir().unwrap();
        let config = signing_key_config(dir.path(), source(dir.path(), &"x".repeat(32)));

        boundary.validate(&config).unwrap();
    }
}

#[test]
fn test_token_realm_boundaries_reject_a_short_environment_signing_key() {
    const CHILD_FLAG: &str = "PERYX_TEST_SHORT_SIGNING_KEY_CHILD";
    const KEY_ENV: &str = "PERYX_TEST_SHORT_SIGNING_KEY";
    if std::env::var_os(CHILD_FLAG).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::server_tests::test_token_realm_boundaries_reject_a_short_environment_signing_key",
            ])
            .env(CHILD_FLAG, "1")
            .env(KEY_ENV, "x".repeat(31))
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config = signing_key_config(dir.path(), SecretSource::Env(KEY_ENV.to_owned()));

    for boundary in CONFIG_BOUNDARIES {
        assert_eq!(
            boundary.validate(&config).unwrap_err().to_string(),
            "`auth.signing_key` must contain at least 32 bytes"
        );
    }
}

#[derive(Clone, Copy)]
enum ConfigBoundary {
    Startup,
    CheckConfig,
}

type SigningKeySource = fn(&Path, &str) -> SecretSource;

const CONFIG_BOUNDARIES: [ConfigBoundary; 2] = [ConfigBoundary::Startup, ConfigBoundary::CheckConfig];
const SIGNING_KEY_CASES: [(ConfigBoundary, SigningKeySource); 4] = [
    (ConfigBoundary::Startup, literal_signing_key),
    (ConfigBoundary::Startup, file_signing_key),
    (ConfigBoundary::CheckConfig, literal_signing_key),
    (ConfigBoundary::CheckConfig, file_signing_key),
];

impl ConfigBoundary {
    fn validate(self, config: &Config) -> anyhow::Result<()> {
        self.validate_with_plugins(config, &plugins())
    }

    fn validate_with_plugins(
        self,
        config: &Config,
        plugins: &peryx_plugin_registry::PluginRegistry,
    ) -> anyhow::Result<()> {
        match self {
            Self::Startup => build_state_with_plugins(config, plugins).map(drop),
            Self::CheckConfig => check_config_with_plugins(config, plugins),
        }
    }
}

fn literal_signing_key(_: &Path, secret: &str) -> SecretSource {
    SecretSource::Literal(secret.to_owned())
}

fn file_signing_key(dir: &Path, secret: &str) -> SecretSource {
    let path = dir.join("signing-key");
    std::fs::write(&path, secret).unwrap();
    SecretSource::File(path)
}

fn signing_key_config(data_dir: &Path, signing_key: SecretSource) -> Config {
    Config {
        data_dir: data_dir.to_owned(),
        auth: AuthConfig {
            signing_key: Some(signing_key),
            ..AuthConfig::default()
        },
        ..neutral_config()
    }
}

fn ldap_provider(bind: LdapBindConfig) -> LdapProviderConfig {
    LdapProviderConfig {
        id: ProviderId::new("staff").unwrap(),
        url: url::Url::parse("ldap://127.0.0.1:9").unwrap(),
        base_dn: "ou=people,dc=example,dc=com".to_owned(),
        bind,
        subject_attribute: "entryUUID".to_owned(),
        display_name_attribute: "displayName".to_owned(),
        group_attribute: None,
        ca_file: None,
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(1),
        max_connections: NonZeroU32::new(2).unwrap(),
        group_mappings: Vec::new(),
    }
}

fn oidc_provider(id: &str, client_secret: Option<SecretSource>) -> OidcProviderConfig {
    OidcProviderConfig {
        id: ProviderId::new(id).unwrap(),
        issuer: "https://idp.example/realms/main".to_owned(),
        client_id: "peryx".to_owned(),
        client_secret,
        redirect_uri: url::Url::parse("https://registry.example/oidc/callback").unwrap(),
        scopes: vec!["openid".to_owned()],
        subject_claim: "sub".to_owned(),
        display_name_claim: "name".to_owned(),
        groups_claim: None,
        clock_skew: Duration::from_mins(1),
        request_timeout: Duration::from_secs(5),
        group_mappings: Vec::new(),
    }
}

#[test]
fn test_build_state_installs_lazy_named_oidc_logins() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            oidc_providers: vec![
                oidc_provider("corporate", Some(SecretSource::Literal("client-secret".to_owned()))),
                oidc_provider("partners", None),
            ],
            signing_key: Some(SecretSource::Literal(TOKEN_REALM_SIGNING_KEY.to_owned())),
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(
        state.serving.oidc_login("corporate").unwrap().id().as_str(),
        "corporate"
    );
    assert_eq!(state.serving.oidc_login("partners").unwrap().id().as_str(), "partners");
    assert!(state.serving.oidc_login("missing").is_none());
    assert_eq!(state.serving.oidc_providers(), vec!["corporate", "partners"]);
}

#[test]
fn test_build_state_reports_an_unreadable_oidc_client_secret() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            oidc_providers: vec![oidc_provider(
                "corporate",
                Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/oidc-secret"))),
            )],
            signing_key: Some(SecretSource::Literal(TOKEN_REALM_SIGNING_KEY.to_owned())),
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let error = build_state(&config).err().expect("expected OIDC client secret error");

    assert!(error.to_string().contains("read OIDC provider corporate client secret"));
}

#[test]
fn test_build_state_rejects_an_invalid_oidc_provider() {
    let dir = tempfile::tempdir().unwrap();
    let mut provider = oidc_provider("corporate", None);
    provider.issuer = "https://idp.example/?tenant=main".to_owned();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            oidc_providers: vec![provider],
            signing_key: Some(SecretSource::Literal(TOKEN_REALM_SIGNING_KEY.to_owned())),
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let error = build_state(&config)
        .err()
        .expect("expected invalid OIDC provider error");

    assert_eq!(error.to_string(), "configure OIDC provider corporate");
}

#[test]
fn test_build_state_installs_lazy_named_ldap_logins() {
    let dir = tempfile::tempdir().unwrap();
    let mut search = ldap_provider(LdapBindConfig::Search {
        username_attribute: "uid".to_owned(),
        bind_dn: "cn=service,dc=example,dc=com".to_owned(),
        bind_password: SecretSource::Literal("directory-secret".to_owned()),
    });
    search.id = ProviderId::new("contractors").unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            ldap_providers: vec![
                ldap_provider(LdapBindConfig::Direct {
                    dn_attribute: "uid".to_owned(),
                }),
                search,
            ],
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(state.serving.ldap_login("staff").unwrap().id().as_str(), "staff");
    assert_eq!(
        state.serving.ldap_login("contractors").unwrap().id().as_str(),
        "contractors"
    );
    assert!(state.serving.ldap_login("missing").is_none());
}

#[test]
fn test_build_state_reports_an_unreadable_ldap_bind_password() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            ldap_providers: vec![ldap_provider(LdapBindConfig::Search {
                username_attribute: "uid".to_owned(),
                bind_dn: "cn=service,dc=example,dc=com".to_owned(),
                bind_password: SecretSource::File(PathBuf::from("/nonexistent/peryx/ldap-password")),
            })],
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let error = build_state(&config).err().expect("expected LDAP bind password error");

    assert!(error.to_string().contains("read LDAP provider staff bind password"));
}

#[test]
fn test_build_state_rejects_an_invalid_ldap_ca() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ldap-ca.pem");
    std::fs::write(&ca, "not a certificate").unwrap();
    let mut provider = ldap_provider(LdapBindConfig::Direct {
        dn_attribute: "uid".to_owned(),
    });
    provider.ca_file = Some(ca);
    let config = Config {
        data_dir: dir.path().join("data"),
        auth: AuthConfig {
            ldap_providers: vec![provider],
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let error = build_state(&config).err().expect("expected invalid LDAP CA error");

    assert_eq!(error.to_string(), "configure LDAP provider staff");
}

#[test]
fn test_build_state_bounds_ldap_ca_reads() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ldap-ca.pem");
    std::fs::write(&ca, vec![b'x'; (1 << 20) + 1]).unwrap();
    let mut provider = ldap_provider(LdapBindConfig::Direct {
        dn_attribute: "uid".to_owned(),
    });
    provider.ca_file = Some(ca);
    let config = Config {
        data_dir: dir.path().join("data"),
        auth: AuthConfig {
            ldap_providers: vec![provider],
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let error = build_state(&config).err().expect("expected oversized LDAP CA error");

    assert_eq!(error.to_string(), "read LDAP provider staff CA");
}

#[test]
fn test_build_state_reports_an_unreadable_ldap_ca() {
    let dir = tempfile::tempdir().unwrap();
    let mut provider = ldap_provider(LdapBindConfig::Direct {
        dn_attribute: "uid".to_owned(),
    });
    provider.ca_file = Some(PathBuf::from("/nonexistent/peryx/ldap-ca.pem"));
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            ldap_providers: vec![provider],
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let error = build_state(&config).err().expect("expected LDAP CA read error");

    assert_eq!(error.to_string(), "read LDAP provider staff CA");
}

#[test]
fn test_build_state_reports_an_unreadable_signing_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            signing_key: Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/signing-key"))),
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let err = build_state(&config).err().expect("expected signing-key read error");

    assert_eq!(err.to_string(), "read `auth.signing_key`");
}

#[rstest]
#[case::literal(empty_literal_signing_key, "`auth.signing_key` must not be empty")]
#[case::file(empty_file_signing_key, "read `auth.signing_key`")]
fn test_build_state_rejects_an_empty_signing_key(#[case] source: fn(&Path) -> SecretSource, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("data"),
        auth: AuthConfig {
            signing_key: Some(source(dir.path())),
            ..AuthConfig::default()
        },
        ..neutral_config()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected empty signing-key error");
    };

    assert_eq!(err.to_string(), expected);
}

fn empty_literal_signing_key(_: &Path) -> SecretSource {
    SecretSource::Literal(" \n".to_owned())
}

fn empty_file_signing_key(dir: &Path) -> SecretSource {
    let path = dir.join("signing-key");
    std::fs::write(&path, " \n").unwrap();
    SecretSource::File(path)
}

#[test]
fn test_build_router_data_dir_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blocker");
    std::fs::write(&file, "x").unwrap();
    let config = Config {
        data_dir: file.join("sub"),
        ..neutral_config()
    };
    let err = build_router(&config).unwrap_err();
    assert!(err.to_string().contains("create data directory"));
}

#[test]
fn test_check_config_reports_duplicate_names_and_routes() {
    let dir = tempfile::tempdir().unwrap();
    let mut duplicate_name = parsed_config(&dir, "[[index]]\nname = \"hosted\"\nhosted = true\n");
    duplicate_name.indexes.push(duplicate_name.indexes[0].clone());
    assert_eq!(
        check_config(&duplicate_name).unwrap_err().to_string(),
        "duplicate index name hosted"
    );

    let mut duplicate_route = duplicate_name;
    duplicate_route.indexes[1].name = "other".to_owned();
    assert_eq!(
        check_config(&duplicate_route).unwrap_err().to_string(),
        "duplicate index route hosted"
    );
}

#[rstest]
#[case::pypi("root-pypi", "root/pypi")]
#[case::oci("root-oci", "root/oci")]
fn test_default_virtual_indexes_separate_names_from_routes(#[case] name: &str, #[case] route: &str) {
    let config = Config::default();

    assert!(
        config
            .indexes
            .iter()
            .any(|index| index.name == name && index.route == route)
    );
}

#[rstest]
#[case::startup(start_config)]
#[case::check_config(check_config)]
fn test_configuration_boundaries_reject_a_nested_index_name(#[case] validate: fn(&Config) -> anyhow::Result<()>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = parsed_config(&dir, "[[index]]\nname = \"hosted\"\nhosted = true\n");
    config.indexes[0].name = "root/pypi".to_owned();

    assert_eq!(
        validate(&config).unwrap_err().to_string(),
        "invalid index name \"root/pypi\": path parameters must be non-empty segments without separators, traversal, or control characters"
    );
}

fn start_config(config: &Config) -> anyhow::Result<()> {
    build_state(config).map(drop)
}

#[test]
fn test_check_config_uses_compiled_plugins() {
    let mut config = neutral_config();
    config.indexes[0].ecosystem = peryx_core::Ecosystem::new("missing");

    assert_eq!(
        crate::server::check_config(&config).unwrap_err().to_string(),
        "activate configured ecosystems"
    );
}

#[rstest]
#[case::startup(start_config)]
#[case::check_config(check_config)]
fn test_configuration_boundaries_reject_ui_routes(#[case] validate: fn(&Config) -> anyhow::Result<()>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral_config()
    };
    config.indexes[0].route = "login".to_owned();

    assert_eq!(
        validate(&config).unwrap_err().to_string(),
        "invalid index route login: invalid route \"login\": prefix \"/login\" is reserved by peryx UI"
    );
}

#[test]
fn test_registry_without_pypi_accepts_upload_route() {
    let mut config = neutral_config();
    config.indexes[0].route = "upload".to_owned();

    build_indexes_with_plugins(&config.indexes, &config.auth, false, &plugins()).unwrap();
}

#[rstest]
#[case::pypi("pypi", "upload", "/upload")]
#[case::oci("oci", "v2", "/v2/")]
fn test_compiled_plugins_reject_owned_routes(#[case] ecosystem: &str, #[case] route: &str, #[case] prefix: &str) {
    let plugins = crate::compiled_plugins();
    let mut config = Config::with_plugins(&plugins);
    config.indexes.retain(|index| index.ecosystem.as_str() == ecosystem);
    config.indexes[0].route = route.to_owned();

    assert_eq!(
        check_config_with_plugins(&config, &plugins).unwrap_err().to_string(),
        format!("invalid index route {route}: invalid route {route:?}: prefix {prefix:?} is reserved by {ecosystem}")
    );
}

#[test]
fn test_check_config_reports_route_acl_and_upstream_secret_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut invalid_route = parsed_config(&dir, "[[index]]\nname = \"hosted\"\nhosted = true\n");
    invalid_route.indexes[0].route = "../hosted".to_owned();
    assert_eq!(
        check_config(&invalid_route).unwrap_err().to_string(),
        "invalid index route ../hosted"
    );

    let invalid_acl = parsed_config(
        &dir,
        "[[index]]\nname = \"hosted\"\nhosted = true\n[[index.access_token]]\nname = \"client\"\nsecret_file = \
         \"/nonexistent/peryx/token\"\nactions = [\"read\"]\n",
    );
    assert!(
        check_config(&invalid_acl)
            .unwrap_err()
            .to_string()
            .starts_with("read the access rules of index hosted")
    );

    let invalid_upstream_secret = parsed_config(
        &dir,
        "[[index]]\nname = \"cached\"\n[[index.upstream]]\nname = \"primary\"\nurl = \
         \"https://upstream.example/\"\ntoken_file = \"/nonexistent/peryx/token\"\n",
    );
    assert!(
        check_config(&invalid_upstream_secret)
            .unwrap_err()
            .to_string()
            .starts_with("read the upstream credentials of index cached")
    );
}

#[test]
fn test_check_config_reports_invalid_cached_and_virtual_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let invalid_upstream = parsed_config(
        &dir,
        "[[index]]\nname = \"cached\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"not a URL\"\n",
    );
    let error = check_config(&invalid_upstream).unwrap_err();
    assert_eq!(
        error.to_string(),
        "build cached index cached with upstream <invalid upstream URL>"
    );

    let unknown_layer = parsed_config(&dir, "[[index]]\nname = \"root\"\nlayers = [\"missing\"]\n");
    assert_eq!(
        check_config(&unknown_layer).unwrap_err().to_string(),
        "virtual index root references unknown index missing"
    );

    let cached_target = parsed_config(
        &dir,
        "[[index]]\nname = \"cached\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://upstream.example/\"\n\
         [[index]]\nname = \"root\"\nlayers = [\"cached\"]\nwrite_target = \"cached\"\n",
    );
    assert_eq!(
        check_config(&cached_target).unwrap_err().to_string(),
        "virtual index root write target cached is not a hosted index"
    );
}

#[test]
fn test_build_state_rejects_an_invalid_secondary_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let config = parsed_config(
        &dir,
        "[[index]]\nname = \"cached\"\n[[index.upstream]]\nname = \"primary\"\nurl = \
         \"https://primary.example/\"\n[[index.upstream]]\nname = \"secondary\"\nurl = \"not a URL\"\n",
    );

    let error = build_state(&config).err().expect("expected invalid secondary upstream");

    assert_eq!(
        error.to_string(),
        "build cached index cached with upstream <invalid upstream URL>"
    );
}

#[test]
fn test_build_state_rejects_an_invalid_artifact_url_with_netrc() {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("credentials.netrc");
    std::fs::write(&netrc, "machine primary.example login reader password secret\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&netrc, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config = parsed_config(
        &dir,
        &format!(
            "netrc = {:?}\n[[index]]\nname = \"cached\"\n[[index.upstream]]\nname = \"primary\"\nurl = \
             \"https://primary.example/\"\nartifact_url = \"not a URL\"\n",
            netrc.display().to_string()
        ),
    );

    let error = build_state(&config).err().expect("expected invalid artifact URL");

    assert!(error.to_string().contains("match netrc credentials"), "{error:#}");
}

#[test]
fn test_build_state_selects_an_implicit_hosted_write_target() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&parsed_config(
        &dir,
        "[[index]]\nname = \"hosted\"\nhosted = true\n\
         [[index]]\nname = \"root\"\nlayers = [\"hosted\"]\n",
    ))
    .unwrap();

    assert!(matches!(
        state.serving.indexes[1].kind,
        peryx_driver::IndexKind::Virtual {
            write_target: Some(0),
            ..
        }
    ));
}

#[test]
fn test_build_state_applies_normalized_upstream_routes() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&parsed_config(
        &dir,
        r#"
[[index]]
name = "cache"
protected = ["Internal.Project"]

[index.pins]
"Pinned.Project" = "secondary"

[index.policy]
block_resources = ["Blocked.Project"]

[[index.upstream]]
name = "primary"
url = "https://primary.example/catalog/"
artifact_url = "https://artifacts.example/files/"
token = "token"

[[index.upstream]]
name = "secondary"
url = "https://secondary.example/catalog/"
artifact_url = "https://public-artifacts.example/files/"
"#,
    ))
    .unwrap();
    let route = &state.serving.upstream_routes["cache"];

    assert_eq!(
        route
            .candidates("internal.project")
            .map(peryx_upstream::NamedUpstream::name)
            .collect::<Vec<_>>(),
        ["primary"]
    );
    assert_eq!(
        route
            .candidates("pinned.project")
            .map(peryx_upstream::NamedUpstream::name)
            .collect::<Vec<_>>(),
        ["secondary"]
    );
    assert!(
        state.serving.indexes[0]
            .policy
            .check_resource(PolicyAction::Cached, "blocked.project")
            .is_err()
    );
}

#[tokio::test]
async fn test_build_state_records_policy_decisions_and_rejects_oversized_records() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&parsed_config(&dir, "[[index]]\nname = \"hosted\"\nhosted = true\n")).unwrap();

    state.serving.indexes[0]
        .policy
        .check_resource(PolicyAction::Upload, "package")
        .unwrap();
    assert_eq!(
        state
            .serving
            .meta
            .query_policy_decisions(&PolicyDecisionQuery::default())
            .unwrap()
            .decisions[0]
            .record
            .resource,
        "package"
    );

    state.serving.indexes[0]
        .policy
        .check_resource(PolicyAction::Upload, &"x".repeat(513))
        .unwrap();
    assert_eq!(
        state
            .serving
            .meta
            .query_policy_decisions(&PolicyDecisionQuery::default())
            .unwrap()
            .decisions
            .len(),
        1
    );
    assert_eq!(state.serving.meta.policy_input_generation("hosted").unwrap().policy, 1);
}

#[test]
fn test_build_state_keeps_read_only_policy_generation_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = parsed_config(&dir, "[[index]]\nname = \"hosted\"\nhosted = true\n");
    config.read_only = true;

    let state = build_state(&config).unwrap();

    assert_eq!(state.serving.meta.policy_input_generation("hosted").unwrap().policy, 0);
}

#[rstest]
#[case::allow("allowed", true)]
#[case::deny("blocked", false)]
fn test_build_state_enforces_read_only_policy_without_recording(#[case] resource: &str, #[case] allowed: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = parsed_config(
        &dir,
        "[[index]]\nname = \"hosted\"\nhosted = true\n[index.policy]\nblock_resources = [\"blocked\"]\n",
    );
    config.read_only = true;
    let state = build_state(&config).unwrap();

    let result = state.serving.indexes[0]
        .policy
        .check_resource(PolicyAction::Upload, resource);

    assert_eq!(result.is_ok(), allowed);
    assert!(
        state
            .serving
            .meta
            .query_policy_decisions(&PolicyDecisionQuery::default())
            .unwrap()
            .decisions
            .is_empty()
    );
}

#[tokio::test]
async fn test_build_state_starts_literal_and_environment_webhooks() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&parsed_config(
        &dir,
        r#"
[[index]]
name = "hosted"
hosted = true

[[index.webhook]]
name = "literal"
url = "https://hooks.example/literal"
secret = "test-webhook-signing-secret-32-bytes"

[[index.webhook]]
name = "environment"
url = "https://hooks.example/environment"
secret_env = "PATH"
"#,
    ))
    .unwrap();

    assert!(!state.serving.webhooks.is_empty());
}

#[rstest]
#[case::below_minimum(31, false)]
#[case::minimum(32, true)]
fn test_webhook_boundaries_enforce_literal_secret_length(#[case] length: usize, #[case] valid: bool) {
    for boundary in CONFIG_BOUNDARIES {
        let dir = tempfile::tempdir().unwrap();
        let secret = "z".repeat(length);
        let result = boundary.validate(&webhook_config(&dir, WebhookSecret::Literal(secret.clone())));

        if valid {
            result.unwrap();
        } else {
            assert_webhook_secret_error(result, &secret);
        }
    }
}

#[test]
fn test_webhook_boundaries_enforce_environment_secret_length() {
    const SECRET_ENV: &str = "PERYX_TEST_WEBHOOK_SECRET_BOUNDARY";
    if let Ok(secret) = std::env::var(SECRET_ENV) {
        for boundary in CONFIG_BOUNDARIES {
            let dir = tempfile::tempdir().unwrap();
            let result = boundary.validate(&webhook_config(&dir, WebhookSecret::Env(SECRET_ENV.to_owned())));
            if secret.len() >= 32 {
                result.unwrap();
            } else {
                assert_webhook_secret_error(result, &secret);
            }
        }
        return;
    }

    for length in [31, 32] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::server_tests::test_webhook_boundaries_enforce_environment_secret_length",
            ])
            .env(SECRET_ENV, "z".repeat(length))
            .status()
            .unwrap();
        assert!(status.success());
    }
}

fn assert_webhook_secret_error(result: anyhow::Result<()>, secret: &str) {
    let error = format!("{:#}", result.unwrap_err());
    assert_eq!(
        (error.as_str(), error.contains(secret),),
        (
            "build webhook targets: webhook target ci on index hosted secret must contain at least 32 bytes",
            false,
        )
    );
}

fn webhook_config(dir: &tempfile::TempDir, secret: WebhookSecret) -> Config {
    let mut config = neutral_config();
    config.data_dir = dir.path().to_path_buf();
    config.indexes[0].name = "hosted".to_owned();
    config.indexes[0].webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "https://hooks.example/ci".to_owned(),
        secret,
        events: Vec::new(),
    });
    config
}

#[test]
fn test_check_config_reports_a_missing_webhook_environment_secret() {
    let dir = tempfile::tempdir().unwrap();
    let config = parsed_config(
        &dir,
        r#"
[[index]]
name = "hosted"
hosted = true

[[index.webhook]]
name = "ci"
url = "https://hooks.example/ci"
secret_env = "PERYX_TEST_MISSING_WEBHOOK_SECRET"
"#,
    );

    assert_eq!(
        check_config(&config).unwrap_err().to_string(),
        "read webhook secret env var PERYX_TEST_MISSING_WEBHOOK_SECRET for target ci"
    );
}

#[rstest]
#[case("download")]
#[case("manifest-push")]
fn test_webhook_boundaries_reject_event_the_pypi_owner_does_not_emit(#[case] event: &str) {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::compiled_plugins();

    for boundary in CONFIG_BOUNDARIES {
        assert_eq!(
            format!(
                "{:#}",
                boundary
                    .validate_with_plugins(&owner_webhook_config(&dir, "pypi", &[event]), &plugins)
                    .unwrap_err()
            ),
            format!("build webhook targets: unknown webhook event {event:?}")
        );
    }
}

#[rstest]
#[case("pypi", &["delete", "restore", "unyank", "upload", "yank"])]
#[case("oci", &["blob-delete", "manifest-delete", "manifest-push", "manifest-restore"])]
fn test_check_config_accepts_every_webhook_event_the_owner_emits(#[case] ecosystem: &str, #[case] events: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::compiled_plugins();

    check_config_with_plugins(&owner_webhook_config(&dir, ecosystem, events), &plugins).unwrap();
}

#[test]
fn test_build_state_uses_netrc_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let netrc = dir.path().join("credentials.netrc");
    std::fs::write(&netrc, "machine upstream.example login reader password secret\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&netrc, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let state = build_state(&parsed_config(
        &dir,
        &format!(
            "netrc = {:?}\n[[index]]\nname = \"cache\"\n[[index.upstream]]\nname = \"primary\"\nurl = \
             \"https://upstream.example/catalog/\"\n",
            netrc.display().to_string()
        ),
    ))
    .unwrap();

    assert_eq!(
        cache_client(&state.serving).current_credential().unwrap().auth(),
        &Auth::Basic {
            username: "reader".to_owned(),
            password: "secret".to_owned(),
        }
    );
}

#[test]
fn test_build_state_uses_inline_basic_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&parsed_config(
        &dir,
        "[[index]]\nname = \"cache\"\n[[index.upstream]]\nname = \"primary\"\nurl = \
         \"https://upstream.example/catalog/\"\nusername = \"reader\"\npassword = \"secret\"\n",
    ))
    .unwrap();

    assert_eq!(
        cache_client(&state.serving).current_credential().unwrap().auth(),
        &Auth::Basic {
            username: "reader".to_owned(),
            password: "secret".to_owned(),
        }
    );
}

#[tokio::test]
async fn test_build_state_refreshes_file_credentials_after_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "old").unwrap();
    let state = build_state(&refreshing_config(&dir, &token, "fail")).unwrap();
    let generation = cache_client(&state.serving).current_credential().unwrap().generation();
    std::fs::write(&token, "new").unwrap();

    let refreshed = cache_client(&state.serving)
        .auth()
        .refresh_after_unauthorized(generation)
        .await
        .unwrap();

    assert_eq!(refreshed.auth(), &Auth::Bearer("new".to_owned()));
    assert_eq!(refreshed.generation(), generation + 1);
}

#[rstest]
#[case::fail("fail", Some("index cache:"))]
#[case::anonymous("anonymous", None)]
#[tokio::test]
async fn test_build_state_applies_credential_refresh_failure_mode(
    #[case] failure: &str,
    #[case] expected_error: Option<&str>,
) {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "old").unwrap();
    let state = build_state(&refreshing_config(&dir, &token, failure)).unwrap();
    let peryx_driver::IndexKind::Cached { client, .. } = &state.serving.indexes[0].kind else {
        panic!("expected cached index");
    };
    let generation = client.current_credential().unwrap().generation();
    std::fs::remove_file(&token).unwrap();

    let refreshed = client.auth().refresh_after_unauthorized(generation).await;

    match expected_error {
        Some(expected) => assert!(refreshed.unwrap_err().to_string().starts_with(expected)),
        None => assert_eq!(refreshed.unwrap().auth(), &Auth::None),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn test_build_state_runs_an_exec_credential_provider() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("credential-helper");
    std::fs::write(
        &helper,
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s' \
         '{\"version\":1,\"expires_at\":\"2099-01-01T00:00:00Z\",\"type\":\"bearer\",\"token\":\"exec-token\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = build_state(&parsed_config(
        &dir,
        &format!(
            "[[index]]\nname = \"cache\"\n[[index.upstream]]\nname = \"primary\"\nurl = \
             \"https://upstream.example/catalog/\"\n[index.upstream.credential_exec]\nargv = [{:?}]\n",
            helper.display().to_string()
        ),
    ))
    .unwrap();

    assert_eq!(
        cache_client(&state.serving).auth().credential().await.unwrap().auth(),
        &Auth::Bearer("exec-token".to_owned())
    );
}

fn cache_client(state: &peryx_driver::state::ServingState) -> &peryx_upstream::UpstreamClient {
    state.upstream_routes["cache"]
        .candidates("resource")
        .next()
        .expect("cached index has a primary upstream")
        .client()
}

fn dc_member(node: &str, dc: &str, address: &str, role: DcRole) -> DcMember {
    DcMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: address.to_owned(),
        role,
    }
}

#[test]
fn test_build_state_uses_node_identity_for_the_local_datacenter() {
    let dir = tempfile::tempdir().unwrap();
    claim_writer(&dir, "east-writer");
    let config = Config {
        writer_identity: Some("east-writer".to_owned()),
        node_identity: Some("west-replica".to_owned()),
        availability: AvailabilityConfig::Ha(ReplicationConfig::Replica {
            upstream: "https://writer.example/".to_owned(),
            token: SecretSource::Literal("secret".to_owned()),
            poll_interval: Duration::from_secs(1),
            page_size: NonZeroUsize::MIN,
        }),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members: vec![
                dc_member("east-writer", "east", "http://east/", DcRole::Writer),
                dc_member("west-replica", "west", "https://west/", DcRole::Replica),
            ],
        }),
        ..s3_blob_config(&dir)
    };

    assert_eq!(
        build_state(&config).unwrap().serving.availability_role(),
        peryx_core::NodeRole::Replica
    );
}

#[test]
fn test_member_transports_follow_the_configured_topology() {
    let dc_dir = tempfile::tempdir().unwrap();
    let ha_dir = tempfile::tempdir().unwrap();
    let replication = ReplicationConfig::Primary {
        source: "writer".to_owned(),
        token: SecretSource::Literal("token".to_owned()),
    };
    let members = vec![
        dc_member("local", "east", "http://127.0.0.1:8000", DcRole::Writer),
        dc_member("peer", "east", "http://127.0.0.1:8001", DcRole::Replica),
        dc_member("west", "west", "https://127.0.0.1:8002", DcRole::Replica),
    ];
    let membership = Some(DcMembership {
        group: "group".to_owned(),
        members,
    });
    let dc = Config {
        data_dir: dc_dir.path().to_path_buf(),
        availability: AvailabilityConfig::Dc(replication.clone()),
        dc_membership: membership.clone(),
        writer_identity: Some("local".to_owned()),
        ..neutral_config()
    };
    let ha = Config {
        data_dir: ha_dir.path().to_path_buf(),
        availability: AvailabilityConfig::Ha(replication),
        dc_membership: membership,
        node_identity: Some("local".to_owned()),
        writer_identity: Some("local".to_owned()),
        ..neutral_config()
    };

    assert_eq!(
        build_state(&dc).unwrap().serving.availability_topology().members.len(),
        3
    );
    assert_eq!(
        build_state(&ha).unwrap().serving.availability_topology().members.len(),
        3
    );
}

#[rstest]
#[case::same_datacenter(
    AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "writer".to_owned(),
        token: SecretSource::Literal("token".to_owned()),
    }),
    "east",
)]
#[case::remote_datacenter(
    AvailabilityConfig::Ha(ReplicationConfig::Primary {
        source: "writer".to_owned(),
        token: SecretSource::Literal("token".to_owned()),
    }),
    "west",
)]
fn test_build_state_rejects_an_invalid_member_address(#[case] availability: AvailabilityConfig, #[case] peer_dc: &str) {
    let dir = tempfile::tempdir().unwrap();
    let node_identity = matches!(availability, AvailabilityConfig::Ha(_)).then(|| "local".to_owned());
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        availability,
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members: vec![
                dc_member("local", "east", "http://127.0.0.1:8000", DcRole::Writer),
                dc_member("peer", peer_dc, "not a url", DcRole::Replica),
            ],
        }),
        node_identity,
        writer_identity: Some("local".to_owned()),
        ..neutral_config()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected invalid member address");
    };

    assert!(
        format!("{error:#}").contains("member `address` \"not a url\" must be an http or https URL"),
        "{error:#}"
    );
}

#[test]
fn test_member_transports_fall_back_to_the_rostered_writer_without_a_node_identity() {
    let dc_dir = tempfile::tempdir().unwrap();
    let ha_dir = tempfile::tempdir().unwrap();
    let replication = ReplicationConfig::Primary {
        source: "writer".to_owned(),
        token: SecretSource::Literal("token".to_owned()),
    };
    let membership = Some(DcMembership {
        group: "group".to_owned(),
        members: vec![dc_member("writer", "east", "http://127.0.0.1:8000", DcRole::Writer)],
    });
    let dc = Config {
        data_dir: dc_dir.path().to_path_buf(),
        availability: AvailabilityConfig::Dc(replication.clone()),
        dc_membership: membership.clone(),
        writer_identity: Some("writer".to_owned()),
        ..neutral_config()
    };
    let ha = Config {
        data_dir: ha_dir.path().to_path_buf(),
        availability: AvailabilityConfig::Ha(replication),
        dc_membership: membership,
        writer_identity: Some("writer".to_owned()),
        ..neutral_config()
    };

    assert_eq!(
        build_state(&dc).unwrap().serving.availability_topology().local_node,
        Some("writer".to_owned())
    );
    assert_eq!(
        build_state(&ha).unwrap().serving.availability_topology().local_node,
        Some("writer".to_owned())
    );
}

fn neutral_config() -> Config {
    Config::with_plugins(&plugins())
}

fn check_config(config: &Config) -> anyhow::Result<()> {
    check_config_with_plugins(config, &plugins())
}

fn parsed_config(dir: &tempfile::TempDir, text: &str) -> Config {
    let neutral = neutral_config();
    let ecosystem = neutral.indexes[0].ecosystem.clone();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        ..neutral
    }
    .apply(crate::config::from_toml(dir.path().join("peryx.toml"), text).unwrap())
    .unwrap();
    for index in &mut config.indexes {
        index.ecosystem = ecosystem.clone();
    }
    config
}

fn owner_webhook_config(dir: &tempfile::TempDir, ecosystem: &str, events: &[&str]) -> Config {
    let plugins = crate::compiled_plugins();
    let mut config = Config::with_plugins(&plugins)
        .apply_with_plugins(
            crate::config::from_toml(
                dir.path().join("peryx.toml"),
                &format!(
                    "[[index]]\nname = \"hosted\"\necosystem = {ecosystem:?}\nhosted = true\n\
                     [[index.webhook]]\nname = \"ci\"\nurl = \"https://hooks.example/ci\"\n\
                     secret = \"test-webhook-signing-secret-32-bytes\"\nevents = {events:?}\n"
                ),
            )
            .unwrap(),
            &plugins,
        )
        .unwrap();
    config.data_dir = dir.path().to_path_buf();
    config
}

fn refreshing_config(dir: &tempfile::TempDir, token: &Path, failure: &str) -> Config {
    parsed_config(
        dir,
        &format!(
            "[[index]]\nname = \"cache\"\n[[index.upstream]]\nname = \"primary\"\nurl = \
             \"https://upstream.example/catalog/\"\ntoken_file = {:?}\ncredential_refresh_secs = 3600\n\
             credential_refresh_on_unauthorized = true\ncredential_failure = {failure:?}\n",
            token.display().to_string()
        ),
    )
}

fn build_state(config: &Config) -> anyhow::Result<Arc<peryx_driver::AppState>> {
    build_state_with_plugins(config, &plugins())
}

fn build_router(config: &Config) -> anyhow::Result<axum::Router> {
    build_router_with_plugins(config, &plugins())
}

#[test]
fn test_none_mode_builds_no_distributed_state_or_resources() {
    use redb::{ReadableDatabase as _, TableHandle as _};

    const DISTRIBUTED_TABLES: [&str; 18] = [
        "artifact_placement",
        "blob_placement",
        "blob_chunk_digest",
        "blob_reclaim_guard",
        "derived_view_frontier",
        "ingress_intent",
        "ingress_intent_count",
        "ingress_intent_order",
        "ingress_intent_seq",
        "journal",
        "journal_blobs",
        "journal_mutations",
        "reclamation_tombstone",
        "reconcile_backlog",
        "transfer_attempt",
        "transfer_audit",
        "visibility_snapshot",
        "writer",
    ];

    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        availability: AvailabilityConfig::None,
        indexes: Vec::new(),
        ..neutral_config()
    };
    let state = build_state(&config).unwrap();

    assert!(state.serving.ownership_authority().is_none());
    assert!(state.serving.cross_dc_copier().is_none());
    assert!(state.serving.placement_reconciler().is_none());
    assert!(state.serving.blob_reclaimer().is_none());
    let mut metrics = String::new();
    state.write_process_metrics(&mut metrics);
    assert!(!metrics.contains("peryx_ha_"), "{metrics}");
    drop(state);

    let database = redb::Database::open(dir.path().join("peryx.redb")).unwrap();
    let read = database.begin_read().unwrap();
    let tables = read
        .list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect::<std::collections::HashSet<_>>();
    for table in DISTRIBUTED_TABLES {
        assert!(!tables.contains(table), "{table}");
    }
}
