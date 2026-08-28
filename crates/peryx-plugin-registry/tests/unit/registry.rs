use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::{
    OperatorJobDefaults, OperatorJobRequest, PluginAuthRegistration, PluginRegistration, PluginRegistry, RegistryError,
};
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Request as HttpRequest, StatusCode};
use peryx_driver::AppState;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{CompiledEcosystemSettings, JobConfig};
use peryx_driver::state::{Index, IndexDescription, IndexKind};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{
    AccountingClass, MetaStore, MetadataMigration, MetadataRecord, MetadataRecordSet, NewQuotaReservation, QuotaLimits,
};
use rstest::rstest;
use tower::ServiceExt as _;
use utoipa::openapi::PathsBuilder;

use super::support::{
    AuthInstallMarker, MISMATCHED_REGISTRATION, NO_JOBS_REGISTRATION, PRIMARY, PRIMARY_V2_REGISTRATION,
    PRIMARY_V2_UPLOADS_REGISTRATION, RuntimeInstallMarker, SECONDARY, SECONDARY_AUTH, SECONDARY_REGISTRATION,
    SECONDARY_V2_REGISTRATION, SECONDARY_V2_UPLOADS_REGISTRATION, SECONDARY_V20_REGISTRATION, driver_factory_calls,
    registrations, reset_driver_factory_calls,
};

const THIRD: peryx_core::Ecosystem = peryx_core::Ecosystem::new("gamma");

#[test]
fn empty_registration_set_is_rejected() {
    assert_eq!(PluginRegistry::new(Vec::new()).err(), Some(RegistryError::Empty));
}

#[derive(Clone, Copy)]
enum DuplicateCase {
    Ecosystem,
    Priority,
    OperatorJob,
    AuthField,
}

#[rstest]
#[case::ecosystem(DuplicateCase::Ecosystem, RegistryError::DuplicateEcosystem(SECONDARY))]
#[case::priority(DuplicateCase::Priority, RegistryError::DuplicatePriority(10))]
#[case::operator_job(DuplicateCase::OperatorJob, RegistryError::DuplicateOperatorJob("run"))]
#[case::auth_field(DuplicateCase::AuthField, RegistryError::DuplicateAuthField("secondary"))]
fn duplicate_registration_values_are_rejected(#[case] case: DuplicateCase, #[case] expected: RegistryError) {
    let mut registrations = registrations();
    match case {
        DuplicateCase::Ecosystem => registrations[1].registration = &SECONDARY_REGISTRATION,
        DuplicateCase::Priority => registrations[1].priority = registrations[0].priority,
        DuplicateCase::OperatorJob => registrations[0].operator_jobs = registrations[1].operator_jobs,
        DuplicateCase::AuthField => {
            registrations[1].auth = Some(PluginAuthRegistration::Extension {
                auth: &SECONDARY_AUTH,
                fields: &["secondary"],
                defaults: toml::Table::new,
            });
        }
    }
    assert_eq!(PluginRegistry::new(registrations).err(), Some(expected));
}

#[rstest]
#[case::duplicate(
    &SECONDARY_V2_REGISTRATION,
    &PRIMARY_V2_REGISTRATION,
    RegistryError::AbsolutePrefixConflict {
        first_ecosystem: SECONDARY,
        first_prefix: "/v2/",
        second_ecosystem: PRIMARY,
        second_prefix: "/v2/",
    },
)]
#[case::parent_before_child(
    &SECONDARY_V2_REGISTRATION,
    &PRIMARY_V2_UPLOADS_REGISTRATION,
    RegistryError::AbsolutePrefixConflict {
        first_ecosystem: SECONDARY,
        first_prefix: "/v2/",
        second_ecosystem: PRIMARY,
        second_prefix: "/v2/uploads/",
    },
)]
#[case::child_before_parent(
    &SECONDARY_V2_UPLOADS_REGISTRATION,
    &PRIMARY_V2_REGISTRATION,
    RegistryError::AbsolutePrefixConflict {
        first_ecosystem: SECONDARY,
        first_prefix: "/v2/uploads/",
        second_ecosystem: PRIMARY,
        second_prefix: "/v2/",
    },
)]
fn conflicting_absolute_prefixes_are_rejected(
    #[case] secondary: &'static super::support::Registration,
    #[case] primary: &'static super::support::Registration,
    #[case] expected: RegistryError,
) {
    let mut registrations = registrations();
    registrations[0].registration = secondary;
    registrations[1].registration = primary;

    assert_eq!(PluginRegistry::new(registrations).err(), Some(expected));
}

#[test]
fn sibling_absolute_prefix_segments_are_accepted() {
    let mut registrations = registrations();
    registrations[0].registration = &SECONDARY_V20_REGISTRATION;
    registrations[1].registration = &PRIMARY_V2_REGISTRATION;

    assert_eq!(
        PluginRegistry::new(registrations)
            .unwrap()
            .absolute_prefixes()
            .collect::<Vec<_>>(),
        vec![(SECONDARY, "/v20/"), (PRIMARY, "/v2/")]
    );
}

#[test]
fn mismatched_protocol_driver_is_rejected() {
    let mut registrations = registrations();
    registrations[1].registration = &MISMATCHED_REGISTRATION;
    reset_driver_factory_calls();
    assert_eq!(
        PluginRegistry::new(registrations).unwrap().activate([PRIMARY]).err(),
        Some(RegistryError::DriverEcosystem {
            registration: PRIMARY,
            driver: SECONDARY,
        })
    );
}

#[test]
fn construction_does_not_instantiate_drivers() {
    reset_driver_factory_calls();
    let registry = PluginRegistry::new(registrations()).unwrap();

    assert_eq!(driver_factory_calls(), (0, 0));
    assert!(registry.protocol(&PRIMARY).is_none());
    assert!(registry.protocol(&SECONDARY).is_none());
    assert_eq!(
        registry.activated_plugin(PRIMARY).err(),
        Some(RegistryError::InactiveEcosystem(PRIMARY))
    );
}

#[test]
fn activation_instantiates_only_selected_drivers() {
    reset_driver_factory_calls();
    let registry = PluginRegistry::new(registrations())
        .unwrap()
        .activate([PRIMARY])
        .unwrap();

    assert_eq!(driver_factory_calls(), (1, 0));
    assert!(registry.protocol(&PRIMARY).is_some());
    assert!(registry.protocol(&SECONDARY).is_none());
    assert_eq!(
        registry.activated_plugin(PRIMARY).unwrap().driver().ecosystem(),
        PRIMARY
    );
    assert_eq!(
        registry.activated_plugin(SECONDARY).err(),
        Some(RegistryError::MissingEcosystem(SECONDARY))
    );
}

#[test]
fn registry_errors_name_the_conflict() {
    assert_eq!(
        [
            RegistryError::Empty,
            RegistryError::MissingEcosystem(THIRD),
            RegistryError::InactiveEcosystem(PRIMARY),
            RegistryError::DuplicateEcosystem(PRIMARY),
            RegistryError::DuplicatePriority(10),
            RegistryError::DuplicateOperatorJob("run"),
            RegistryError::DuplicateAuthField("token"),
            RegistryError::AbsolutePrefixConflict {
                first_ecosystem: PRIMARY,
                first_prefix: "/v2/",
                second_ecosystem: SECONDARY,
                second_prefix: "/v2/uploads/",
            },
            RegistryError::DriverEcosystem {
                registration: PRIMARY,
                driver: SECONDARY,
            },
        ]
        .map(|error| error.to_string()),
        [
            "at least one ecosystem registration is required",
            "ecosystem gamma is not installed",
            "ecosystem alpha is not active",
            "duplicate ecosystem alpha",
            "duplicate ecosystem priority 10",
            "duplicate operator job command \"run\"",
            "duplicate auth field \"token\"",
            "ecosystems alpha and beta declare conflicting absolute prefixes \"/v2/\" and \"/v2/uploads/\"",
            "ecosystem alpha registration returned a beta protocol driver",
        ]
    );
}

#[test]
fn activation_rejects_an_unregistered_ecosystem() {
    assert_eq!(
        registry().activate([THIRD]).err(),
        Some(RegistryError::MissingEcosystem(THIRD))
    );
}

#[test]
fn activation_accepts_no_ecosystems() {
    reset_driver_factory_calls();
    let registry = PluginRegistry::new(registrations()).unwrap().activate([]).unwrap();
    let (_state_directory, mut state) = state();

    registry.register_activated_capabilities(&mut state.capability_install_context());
    registry
        .install_drivers(&mut state.runtime_install_context().unwrap(), &settings())
        .unwrap();
    let openapi = registry.openapi_paths(PathsBuilder::new()).build();

    assert_eq!(
        (
            driver_factory_calls(),
            registry.default_indexes().count(),
            registry.browse_paths(),
            registry.has_metadata_migrations(),
            registry.default_auth_extensions(),
            registry.protocol(&PRIMARY).is_some(),
            registry.protocol(&SECONDARY).is_some(),
            registry.activated_plugin(PRIMARY).err(),
            state.rate_limit_principal_for(&PRIMARY).is_some(),
            state.serving.plugin_service::<RuntimeInstallMarker>().is_some(),
            openapi.get_path_item("/alpha-extension").is_some(),
            openapi.get_path_item("/beta-extension").is_some(),
        ),
        (
            (0, 0),
            0,
            &[] as &[&str],
            false,
            toml::Table::new(),
            false,
            false,
            Some(RegistryError::MissingEcosystem(PRIMARY)),
            false,
            false,
            false,
            false,
        )
    );
}

#[test]
fn activation_filters_all_derived_capabilities() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut registrations = registrations();
    for registration in &mut registrations {
        let name = if registration.registration.ecosystem() == PRIMARY {
            "primary"
        } else {
            "secondary"
        };
        registration.metadata_migration = Some(migration(name, MigrationResult::Keep, calls.clone()));
    }
    let registry = PluginRegistry::new(registrations).unwrap().activate([PRIMARY]).unwrap();
    let (_store_directory, store) = metadata_store();
    let (_state_directory, mut state) = state();

    registry.migrate_metadata(&store).unwrap();
    registry.register_activated_capabilities(&mut state.capability_install_context());
    registry
        .install_drivers(&mut state.runtime_install_context().unwrap(), &settings())
        .unwrap();
    let openapi = registry.openapi_paths(PathsBuilder::new()).build();

    assert_eq!(
        (
            registry.is_installed(&PRIMARY),
            registry.is_installed(&SECONDARY),
            registry
                .default_indexes()
                .map(|index| index.ecosystem.clone())
                .collect::<Vec<_>>(),
            registry.browse_paths(),
            registry.protocol(&PRIMARY).is_some(),
            registry.protocol(&SECONDARY).is_some(),
            state.rate_limit_principal_for(&PRIMARY).is_some(),
            state.rate_limit_principal_for(&SECONDARY).is_some(),
            openapi.get_path_item("/alpha-extension").is_some(),
            openapi.get_path_item("/beta-extension").is_some(),
            calls.lock().unwrap().clone(),
        ),
        (
            true,
            false,
            vec![PRIMARY],
            &["/browse/shared", "/browse/alpha"][..],
            true,
            false,
            true,
            false,
            true,
            false,
            vec!["primary"],
        )
    );
}

#[rstest]
#[case::primary(PRIMARY, SECONDARY)]
#[case::secondary(SECONDARY, PRIMARY)]
fn activation_selects_registration(#[case] active: peryx_core::Ecosystem, #[case] inactive: peryx_core::Ecosystem) {
    let registry = registry().activate([active.clone()]).unwrap();

    assert_eq!(
        (
            registry.is_installed(&active),
            registry.is_installed(&inactive),
            registry.protocol(&active).is_some(),
            registry.protocol(&inactive).is_some(),
            registry.drivers().get(&active).is_some(),
            registry.drivers().get(&inactive).is_some(),
            registry
                .default_indexes()
                .map(|index| index.ecosystem.clone())
                .collect::<Vec<_>>(),
        ),
        (true, false, true, false, true, false, vec![active])
    );
}

#[test]
fn inactive_auth_fields_are_rejected() {
    let values = toml::Table::from_iter([("primary".to_owned(), toml::Value::Boolean(false))]);

    assert_eq!(
        registry()
            .activate([SECONDARY])
            .unwrap()
            .validate_auth_extensions(&values, true, 60, &[]),
        Err("auth: unknown field `primary`".to_owned())
    );
    assert_eq!(
        registry()
            .activate([PRIMARY])
            .unwrap()
            .validate_auth_extensions(&values, true, 60, &[]),
        Err("alpha auth rejected".to_owned())
    );
}

#[test]
fn lowest_priority_registration_is_the_default() {
    let mut registrations = registrations();
    registrations.reverse();
    assert_eq!(
        PluginRegistry::new(registrations).unwrap().default_ecosystem(),
        SECONDARY
    );
}

#[test]
fn registrations_are_ordered_by_priority() {
    let mut registrations = registrations();
    registrations.reverse();
    assert_eq!(
        PluginRegistry::new(registrations)
            .unwrap()
            .default_indexes()
            .map(|index| index.ecosystem.clone())
            .collect::<Vec<_>>(),
        vec![SECONDARY, PRIMARY]
    );
}

#[test]
fn registry_without_metadata_capability_runs_no_migrations() {
    let directory = tempfile::tempdir().unwrap();
    let store = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let registry = registry();

    assert!(!registry.has_metadata_migrations());
    assert_eq!(registry.migrate_metadata(&store).unwrap(), []);
}

#[test]
fn registry_runs_one_metadata_capability() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let migration = migration("only", MigrationResult::Rewrite, calls.clone());
    let mut registrations = registrations();
    registrations[0].metadata_migration = Some(migration);
    let registry = PluginRegistry::new(registrations).unwrap();
    let (_directory, store) = metadata_store();

    assert!(registry.has_metadata_migrations());
    assert_eq!(registry.dry_run_metadata_migrations(&store).unwrap()[0].rewritten, 1);
    assert_eq!(*calls.lock().unwrap(), ["only"]);
    assert_eq!(registry.migrate_metadata(&store).unwrap()[0].rewritten, 1);
    assert_eq!(*calls.lock().unwrap(), ["only", "only"]);
}

#[test]
fn registry_runs_metadata_capabilities_in_priority_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first = migration("first", MigrationResult::Keep, calls.clone());
    let second = migration("second", MigrationResult::Keep, calls.clone());
    let mut registrations = registrations();
    registrations[0].metadata_migration = Some(first);
    registrations[1].metadata_migration = Some(second);
    registrations.reverse();
    let registry = PluginRegistry::new(registrations).unwrap();
    let (_directory, store) = metadata_store();

    assert_eq!(registry.migrate_metadata(&store).unwrap().len(), 2);
    assert_eq!(*calls.lock().unwrap(), ["first", "second"]);
}

#[test]
fn registry_stops_metadata_migrations_at_a_stable_failure() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let failure = migration("failure", MigrationResult::Fail, calls.clone());
    let skipped = migration("skipped", MigrationResult::Keep, calls.clone());
    let mut registrations = registrations();
    registrations[0].metadata_migration = Some(failure);
    registrations[1].metadata_migration = Some(skipped);
    let registry = PluginRegistry::new(registrations).unwrap();
    let (_directory, store) = metadata_store();

    assert_eq!(
        registry.migrate_metadata(&store).unwrap_err().to_string(),
        "metadata migration \"failure\" failed for QuotaUsage record \"repository\": rejected"
    );
    assert_eq!(*calls.lock().unwrap(), ["failure"]);
}

#[test]
fn installation_queries_match_the_registrations() {
    let registry = registry();
    assert_eq!(
        (
            registry.is_installed(&PRIMARY),
            registry.is_installed(&SECONDARY),
            registry.is_installed(&THIRD),
        ),
        (true, true, false)
    );
}

#[test]
fn default_indexes_include_each_registration() {
    assert_eq!(
        registry()
            .default_indexes()
            .map(|index| (index.name, index.route, index.ecosystem.clone()))
            .collect::<Vec<_>>(),
        vec![("default", "default", SECONDARY), ("default", "default", PRIMARY)]
    );
}

#[test]
fn drivers_include_each_protocol_driver() {
    let registry = registry();
    let driver = registry.drivers().get(&PRIMARY).unwrap();
    let protocol = registry.protocol(&PRIMARY).unwrap().absolute().unwrap();
    assert_eq!(
        (
            driver.ecosystem(),
            protocol.classify_route("entry"),
            registry.discover_index(&PRIMARY, index_description(), None).unwrap(),
            registry.drivers().get(&SECONDARY).unwrap().ecosystem(),
            registry.drivers().get(&THIRD).is_none(),
            registry
                .drivers()
                .present()
                .map(|driver| driver.ecosystem())
                .collect::<Vec<_>>(),
        ),
        (
            PRIMARY,
            RouteClass::Metadata,
            serde_json::Value::String("default".to_owned()),
            SECONDARY,
            true,
            vec![SECONDARY, PRIMARY],
        )
    );
}

#[test]
fn protocol_lookup_preserves_the_protocol_kind() {
    let registry = registry();
    assert_eq!(
        (
            registry.protocol(&PRIMARY).unwrap().absolute().is_some(),
            registry.protocol(&PRIMARY).unwrap().indexed().is_none(),
            registry.protocol(&THIRD).is_none(),
        ),
        (true, true, true)
    );
}

#[tokio::test]
async fn absolute_protocol_callbacks_match_the_registration() {
    let registry = registry();
    let driver = registry.protocol(&PRIMARY).unwrap().absolute().unwrap();
    let (_, state) = state();
    let response = driver
        .serve(Arc::clone(&state.serving), Request::new(Body::empty()))
        .await;
    assert_eq!(
        (driver.prefixes(), response.status()),
        (&[][..], StatusCode::NO_CONTENT)
    );
}

#[test]
fn settings_compilation_returns_the_ecosystem_value() {
    let compiled = registry()
        .compile_index_settings(&PRIMARY, "default", &toml::Table::new())
        .unwrap()
        .unwrap();
    assert_eq!(
        (compiled.ecosystem(), compiled.value::<String>().map(String::as_str)),
        (PRIMARY, Some("default"))
    );
}

#[test]
fn settings_compilation_preserves_an_absent_value() {
    assert!(
        registry()
            .compile_index_settings(&PRIMARY, "optional", &toml::Table::new())
            .unwrap()
            .is_none()
    );
}

#[test]
fn settings_compilation_preserves_plugin_errors() {
    assert_eq!(
        registry()
            .compile_index_settings(&PRIMARY, "", &toml::Table::new())
            .unwrap_err(),
        "index name is empty"
    );
}

#[test]
fn settings_compilation_rejects_an_uninstalled_ecosystem() {
    assert_eq!(
        registry()
            .compile_index_settings(&THIRD, "default", &toml::Table::new())
            .unwrap_err(),
        "ecosystem gamma is not installed"
    );
}

#[test]
fn job_compilation_dispatches_to_the_claiming_ecosystem() {
    let job = registry()
        .compile_job(JobConfig {
            kind: "shared_job",
            settings: &toml::Table::new(),
            indexes: &[],
        })
        .unwrap();
    assert_eq!(
        (job.ecosystem(), job.kind(), job.settings()),
        (PRIMARY, "shared_job", toml::Table::new())
    );
}

#[test]
fn job_compilation_skips_drivers_without_job_capability() {
    let mut registrations = registrations();
    registrations[1].registration = &NO_JOBS_REGISTRATION;
    let job = PluginRegistry::new(registrations)
        .unwrap()
        .activate([PRIMARY, SECONDARY])
        .unwrap()
        .compile_job(JobConfig {
            kind: "duplicate_job",
            settings: &toml::Table::new(),
            indexes: &[],
        })
        .unwrap();

    assert_eq!(job.ecosystem(), SECONDARY);
}

#[test]
fn compiled_job_creates_through_the_public_scheduler_api() {
    let job = registry()
        .compile_job(JobConfig {
            kind: "shared_job",
            settings: &toml::Table::new(),
            indexes: &[],
        })
        .unwrap();
    let (_directory, state) = state();

    let error = peryx_driver::jobs::scheduled_job(&state, &peryx_driver::jobs::ScheduledJob::Plugin(job))
        .err()
        .expect("test factory unexpectedly created a job");
    assert_eq!(error, "test factory does not execute");
}

#[test]
fn duplicate_job_claims_are_rejected() {
    assert_eq!(
        registry()
            .compile_job(JobConfig {
                kind: "duplicate_job",
                settings: &toml::Table::new(),
                indexes: &[],
            })
            .unwrap_err(),
        "job kind \"duplicate_job\" is claimed by multiple ecosystems"
    );
}

#[test]
fn job_compilation_rejects_an_unknown_kind() {
    assert_eq!(
        registry()
            .compile_job(JobConfig {
                kind: "missing",
                settings: &toml::Table::new(),
                indexes: &[],
            })
            .unwrap_err(),
        "unknown job kind \"missing\""
    );
}

#[test]
fn job_compilation_rejects_a_foreign_ecosystem() {
    assert_eq!(
        registry()
            .compile_job(JobConfig {
                kind: "foreign_job",
                settings: &toml::Table::new(),
                indexes: &[],
            })
            .unwrap_err(),
        "ecosystem alpha driver returned a scheduled job for beta"
    );
}

#[test]
fn operator_job_exposes_owner_defaults() {
    assert_eq!(
        (
            registry().operator_job_defaults("run"),
            registry().operator_job_defaults("sync"),
        ),
        (
            Ok(OperatorJobDefaults {
                item_limit: 10,
                concurrency: 2,
                timeout_secs: 30,
            }),
            Ok(OperatorJobDefaults {
                item_limit: 20,
                concurrency: 4,
                timeout_secs: 60,
            }),
        )
    );
}

#[test]
fn operator_jobs_expose_registered_commands_and_defaults() {
    assert_eq!(
        registry().operator_job_commands().collect::<Vec<_>>(),
        [
            (
                "sync",
                OperatorJobDefaults {
                    item_limit: 20,
                    concurrency: 4,
                    timeout_secs: 60,
                },
            ),
            (
                "run",
                OperatorJobDefaults {
                    item_limit: 10,
                    concurrency: 2,
                    timeout_secs: 30,
                },
            ),
        ]
    );
}

#[test]
fn operator_job_compilation_uses_owner_defaults() {
    let job = registry()
        .compile_operator_job(
            "sync",
            OperatorJobRequest {
                target: "default",
                source: None,
                item_limit: None,
                concurrency: None,
                timeout_secs: None,
            },
        )
        .unwrap();
    assert_eq!(
        (job.ecosystem(), job.kind(), job.settings()),
        (
            SECONDARY,
            "sync",
            toml::Table::from_iter([
                ("target".to_owned(), toml::Value::String("default".to_owned())),
                ("source".to_owned(), toml::Value::String(String::new())),
                ("item-limit".to_owned(), toml::Value::String("20".to_owned())),
                ("concurrency".to_owned(), toml::Value::String("4".to_owned())),
                ("timeout-secs".to_owned(), toml::Value::String("60".to_owned())),
            ]),
        )
    );
}

#[test]
fn operator_job_compilation_preserves_explicit_options() {
    let job = registry()
        .compile_operator_job(
            "run",
            OperatorJobRequest {
                target: "primary",
                source: Some("upstream"),
                item_limit: Some(5),
                concurrency: Some(6),
                timeout_secs: Some(7),
            },
        )
        .unwrap();
    assert_eq!(
        (job.ecosystem(), job.kind(), job.settings()),
        (
            PRIMARY,
            "run",
            toml::Table::from_iter([
                ("target".to_owned(), toml::Value::String("primary".to_owned())),
                ("source".to_owned(), toml::Value::String("upstream".to_owned())),
                ("item-limit".to_owned(), toml::Value::String("5".to_owned())),
                ("concurrency".to_owned(), toml::Value::String("6".to_owned())),
                ("timeout-secs".to_owned(), toml::Value::String("7".to_owned())),
            ]),
        )
    );
}

#[test]
fn operator_job_rejects_an_unknown_command() {
    assert_eq!(
        registry().operator_job_defaults("missing").unwrap_err(),
        "unknown operator job command \"missing\""
    );
}

#[test]
fn operator_job_compilation_rejects_an_unknown_command() {
    assert_eq!(
        registry()
            .compile_operator_job(
                "missing",
                OperatorJobRequest {
                    target: "default",
                    source: None,
                    item_limit: None,
                    concurrency: None,
                    timeout_secs: None,
                },
            )
            .unwrap_err(),
        "unknown operator job command \"missing\""
    );
}

#[test]
fn operator_job_compilation_preserves_plugin_errors() {
    assert_eq!(
        registry()
            .compile_operator_job(
                "run",
                OperatorJobRequest {
                    target: "",
                    source: None,
                    item_limit: None,
                    concurrency: None,
                    timeout_secs: None,
                },
            )
            .unwrap_err(),
        "run target is empty"
    );
}

#[test]
fn local_install_receives_only_its_plugin_settings() {
    let (_directory, mut state) = state();
    registry()
        .install_drivers(&mut state.runtime_install_context().unwrap(), &settings())
        .unwrap();
    let marker = state.serving.plugin_service::<RuntimeInstallMarker>().unwrap();
    assert_eq!(marker.mode, "local");
    assert_eq!(marker.settings, ["primary-a", "primary-b"]);
}

#[test]
fn local_install_registers_request_capabilities() {
    let (_directory, mut state) = state();
    let registry = registry();
    registry.register_activated_capabilities(&mut state.capability_install_context());
    registry
        .install_drivers(&mut state.runtime_install_context().unwrap(), &HashMap::new())
        .unwrap();

    assert_eq!(
        state
            .rate_limit_principal_for(&PRIMARY)
            .unwrap()
            .resolve(&state.serving, None, &HeaderMap::new()),
        peryx_identity::Principal::Named {
            subject: "alpha".to_owned(),
        }
    );
    assert_eq!(
        state.client_discovery_for(&PRIMARY).unwrap().client_endpoint("default"),
        "/default/"
    );
}

#[test]
fn absent_request_capabilities_remain_unregistered() {
    let mut registrations = registrations();
    for registration in &mut registrations {
        registration.rate_limit_principal = None;
        registration.client_discovery = None;
    }
    let registry = active_registry(registrations);
    let (_directory, mut state) = state();

    registry
        .install_drivers(&mut state.runtime_install_context().unwrap(), &HashMap::new())
        .unwrap();

    assert!(state.rate_limit_principal_for(&PRIMARY).is_none());
    assert!(state.client_discovery_for(&PRIMARY).is_none());
    assert_eq!(
        registry.client_endpoint(&PRIMARY, "default").unwrap_err(),
        "ecosystem alpha does not provide client discovery"
    );
}

#[test]
fn local_install_preserves_plugin_errors() {
    let (_directory, mut state) = state();
    assert_eq!(
        registry()
            .install_drivers(
                &mut state.runtime_install_context().unwrap(),
                &HashMap::from([(String::new(), CompiledEcosystemSettings::new(PRIMARY, ()),)])
            )
            .unwrap_err(),
        "local install failed"
    );
}

#[test]
fn local_install_without_browse_paths_registers_no_routes() {
    let mut registrations = registrations();
    for registration in &mut registrations {
        registration.browse = None;
    }
    let registry = active_registry(registrations);
    let (_directory, mut state) = state();

    registry
        .install_drivers(&mut state.runtime_install_context().unwrap(), &HashMap::new())
        .unwrap();

    assert_eq!(state.http_routes().count(), 0);
}

#[test]
fn distributed_install_receives_only_its_plugin_settings() {
    let (_directory, mut state) = state();
    registry()
        .install_distributed_drivers(&mut state.distributed_install_context().unwrap(), &settings())
        .unwrap();
    let marker = state.serving.plugin_service::<RuntimeInstallMarker>().unwrap();
    assert_eq!(marker.mode, "distributed");
    assert_eq!(marker.settings, ["primary-a", "primary-b"]);
}

#[test]
fn distributed_install_uses_local_runtime_without_a_distributed_capability() {
    let mut registrations = registrations();
    for registration in &mut registrations {
        registration.distributed_runtime = None;
    }
    let registry = active_registry(registrations);
    let (_directory, mut state) = state();

    registry
        .install_distributed_drivers(&mut state.distributed_install_context().unwrap(), &settings())
        .unwrap();

    let marker = state.serving.plugin_service::<RuntimeInstallMarker>().unwrap();
    assert_eq!(marker.mode, "local");
    assert_eq!(marker.settings, ["primary-a", "primary-b"]);
}

#[test]
fn distributed_install_preserves_plugin_errors() {
    let (_directory, mut state) = state();
    assert_eq!(
        registry()
            .install_distributed_drivers(
                &mut state.distributed_install_context().unwrap(),
                &HashMap::from([(String::new(), CompiledEcosystemSettings::new(PRIMARY, ()),)])
            )
            .unwrap_err(),
        "distributed install failed"
    );
}

#[test]
fn openapi_paths_include_each_registration() {
    let paths = registry().openapi_paths(PathsBuilder::new()).build();
    assert_eq!(
        (
            paths.get_path_item("/alpha-extension").is_some(),
            paths.get_path_item("/beta-extension").is_some(),
        ),
        (true, true)
    );
}

#[test]
fn browse_paths_include_each_unique_registration_path() {
    assert_eq!(
        registry().browse_paths(),
        &["/browse/shared", "/browse/beta", "/browse/alpha"]
    );
}

#[tokio::test]
async fn browse_dispatch_uses_the_registered_capability() {
    let (_directory, state) = state();
    let state = Arc::new(state);
    assert_eq!(
        (
            registry()
                .dispatch_browse(SECONDARY, Arc::clone(&state), Request::new(axum::body::Body::empty()),)
                .await
                .unwrap()
                .status(),
            registry()
                .dispatch_browse(PRIMARY, state, Request::new(axum::body::Body::empty()))
                .await
                .unwrap()
                .status(),
        ),
        (axum::http::StatusCode::CREATED, axum::http::StatusCode::ACCEPTED)
    );
}

#[tokio::test]
async fn browse_dispatch_rejects_an_uninstalled_ecosystem() {
    let (_directory, state) = state();
    assert_eq!(
        registry()
            .dispatch_browse(THIRD, Arc::new(state), Request::new(axum::body::Body::empty()),)
            .await
            .unwrap_err(),
        "ecosystem gamma is not installed"
    );
}

#[tokio::test]
async fn browse_dispatch_rejects_a_missing_capability() {
    let (_directory, state) = state();
    let mut registrations = registrations();
    registrations[1].browse = None;
    assert_eq!(
        PluginRegistry::new(registrations)
            .unwrap()
            .dispatch_browse(PRIMARY, Arc::new(state), Request::new(axum::body::Body::empty()),)
            .await
            .unwrap_err(),
        "ecosystem alpha does not provide browsing"
    );
}

#[tokio::test]
async fn installed_browse_routes_dispatch_by_index_ecosystem() {
    let app = browse_app([index("alpha", PRIMARY), index("beta", SECONDARY)]);

    assert_eq!(
        (
            get(&app, "/browse/shared?index=alpha").await,
            get(&app, "/browse/shared?index=beta").await,
            get(&app, "/browse/alpha?index=alpha").await,
            get(&app, "/browse/beta?index=beta").await,
        ),
        (
            StatusCode::ACCEPTED,
            StatusCode::CREATED,
            StatusCode::ACCEPTED,
            StatusCode::CREATED,
        )
    );
}

#[tokio::test]
async fn installed_browse_routes_reject_invalid_targets() {
    let app = browse_app([index("alpha", PRIMARY), index("orphan", THIRD)]);

    assert_eq!(
        (
            get(&app, "/browse/shared").await,
            get(&app, "/browse/shared?index=missing").await,
            get(&app, "/browse/shared?index=orphan").await,
        ),
        (StatusCode::BAD_REQUEST, StatusCode::NOT_FOUND, StatusCode::NOT_FOUND)
    );
}

#[test]
fn auth_defaults_include_each_owned_field() {
    assert_eq!(
        registry().default_auth_extensions(),
        toml::Table::from_iter([
            ("primary".to_owned(), toml::Value::Boolean(true)),
            ("secondary".to_owned(), toml::Value::Boolean(true)),
        ])
    );
}

#[rstest]
#[case::primary(
    vec![PRIMARY],
    toml::Table::from_iter([("primary".to_owned(), toml::Value::Boolean(true))])
)]
#[case::secondary(
    vec![SECONDARY],
    toml::Table::from_iter([("secondary".to_owned(), toml::Value::Boolean(true))])
)]
#[case::both(
    vec![PRIMARY, SECONDARY],
    toml::Table::from_iter([
        ("primary".to_owned(), toml::Value::Boolean(true)),
        ("secondary".to_owned(), toml::Value::Boolean(true)),
    ])
)]
fn auth_defaults_follow_activation(#[case] ecosystems: Vec<peryx_core::Ecosystem>, #[case] expected: toml::Table) {
    assert_eq!(
        registry().activate(ecosystems).unwrap().default_auth_extensions(),
        expected
    );
}

#[test]
fn auth_validation_applies_active_defaults() {
    let registry = registry();

    assert_eq!(
        registry
            .activate([PRIMARY])
            .unwrap()
            .validate_auth_extensions(&toml::Table::new(), true, 60, &[]),
        Ok(())
    );
    assert_eq!(
        registry.activate([PRIMARY]).unwrap().validate_auth_extensions(
            &toml::Table::from_iter([("primary".to_owned(), toml::Value::Boolean(false))]),
            true,
            60,
            &[],
        ),
        Err("alpha auth rejected".to_owned())
    );
}

#[test]
fn auth_validation_dispatches_owned_fields() {
    assert_eq!(
        registry().validate_auth_extensions(
            &toml::Table::from_iter([
                ("primary".to_owned(), toml::Value::Boolean(true)),
                ("secondary".to_owned(), toml::Value::Boolean(true)),
            ]),
            true,
            60,
            &[],
        ),
        Ok(())
    );
}

#[test]
fn auth_validation_preserves_plugin_errors() {
    assert_eq!(
        registry().validate_auth_extensions(
            &toml::Table::from_iter([("primary".to_owned(), toml::Value::Boolean(false))]),
            true,
            60,
            &[],
        ),
        Err("alpha auth rejected".to_owned())
    );
}

#[test]
fn auth_validation_rejects_unknown_fields() {
    assert_eq!(
        registry().validate_auth_extensions(
            &toml::Table::from_iter([("unknown".to_owned(), toml::Value::Boolean(true))]),
            true,
            60,
            &[],
        ),
        Err("auth: unknown field `unknown`".to_owned())
    );
}

#[test]
fn auth_install_dispatches_owned_fields() {
    let (_directory, mut state) = state();
    registry()
        .install_auth_extensions(
            &mut state.auth_install_context().unwrap(),
            &toml::Table::from_iter([
                ("primary".to_owned(), toml::Value::Boolean(true)),
                ("secondary".to_owned(), toml::Value::Boolean(true)),
            ]),
        )
        .unwrap();
    let marker = state.serving.plugin_service::<AuthInstallMarker>().unwrap();
    assert_eq!(marker.0, PRIMARY);
}

#[test]
fn auth_install_preserves_ecosystem_errors() {
    let (_directory, mut state) = state();
    assert_eq!(
        registry().install_auth_extensions(
            &mut state.auth_install_context().unwrap(),
            &toml::Table::from_iter([("primary".to_owned(), toml::Value::Boolean(false))])
        ),
        Err("alpha auth install failed".to_owned())
    );
}

#[test]
fn snippets_preserve_plugin_output() {
    assert_eq!(
        registry()
            .snippet_text(
                &PRIMARY,
                &BaseUrl::parse("https://registry.example/root").unwrap(),
                "entry",
                true,
                "plain",
            )
            .unwrap(),
        Some("https://registry.example/root/entry?uploads=true&format=plain".to_owned())
    );
}

#[test]
fn snippets_preserve_plugin_errors() {
    assert_eq!(
        registry()
            .snippet_text(
                &PRIMARY,
                &BaseUrl::parse("https://registry.example").unwrap(),
                "entry",
                false,
                "",
            )
            .unwrap_err(),
        "format is empty"
    );
}

#[test]
fn snippets_reject_an_uninstalled_ecosystem() {
    assert_eq!(
        registry()
            .snippet_text(
                &THIRD,
                &BaseUrl::parse("https://registry.example").unwrap(),
                "entry",
                false,
                "plain",
            )
            .unwrap_err(),
        "ecosystem gamma is not installed"
    );
}

#[test]
fn snippets_reject_a_missing_capability() {
    let mut registrations = registrations();
    registrations[1].snippets = None;
    assert_eq!(
        PluginRegistry::new(registrations)
            .unwrap()
            .snippet_text(
                &PRIMARY,
                &BaseUrl::parse("https://registry.example").unwrap(),
                "entry",
                false,
                "plain",
            )
            .unwrap_err(),
        "ecosystem alpha does not provide client snippets"
    );
}

fn registry() -> PluginRegistry {
    active_registry(registrations())
}

fn active_registry(registrations: Vec<PluginRegistration>) -> PluginRegistry {
    PluginRegistry::new(registrations)
        .unwrap()
        .activate([PRIMARY, SECONDARY])
        .unwrap()
}

#[derive(Clone, Copy)]
enum MigrationResult {
    Keep,
    Rewrite,
    Fail,
}

struct TestMigration {
    name: &'static str,
    result: MigrationResult,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl MetadataMigration for TestMigration {
    fn name(&self) -> &'static str {
        self.name
    }

    fn record_sets(&self) -> &[MetadataRecordSet] {
        &[MetadataRecordSet::QuotaUsage]
    }

    fn rewrite(&self, _: MetadataRecordSet, record: &MetadataRecord) -> Result<Option<MetadataRecord>, String> {
        self.calls.lock().unwrap().push(self.name);
        match self.result {
            MigrationResult::Keep => Ok(None),
            MigrationResult::Rewrite => Ok(Some(record.clone())),
            MigrationResult::Fail => Err("rejected".to_owned()),
        }
    }
}

fn migration(
    name: &'static str,
    result: MigrationResult,
    calls: Arc<Mutex<Vec<&'static str>>>,
) -> Arc<dyn MetadataMigration> {
    Arc::new(TestMigration { name, result, calls })
}

fn metadata_store() -> (tempfile::TempDir, MetaStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    store
        .reserve_quota(
            NewQuotaReservation {
                repository: "repository",
                resource: Some("resource"),
                group: Some("group"),
                digest: "digest",
                bytes: 1,
                class: AccountingClass::Hosted,
                created_at_unix: 1,
            },
            QuotaLimits::default(),
        )
        .unwrap();
    (directory, store)
}

fn index_description() -> IndexDescription {
    IndexDescription {
        name: "default".to_owned(),
        route: "default".to_owned(),
        ecosystem: "alpha".to_owned(),
        kind: "default",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: None,
        hosted: None,
    }
}

fn state() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(directory.path().join("blobs"));
    (directory, AppState::new(meta, blobs, 60, Vec::new()))
}

fn browse_app(indexes: impl IntoIterator<Item = Index>) -> Router {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(directory.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, indexes.into_iter().collect());
    registry()
        .install_drivers(&mut state.runtime_install_context().unwrap(), &HashMap::new())
        .unwrap();
    let router = state
        .http_routes()
        .fold(Router::new(), |router, routes| router.merge(routes.routes()));
    router.with_state(Arc::new(state))
}

fn index(route: &str, ecosystem: peryx_core::Ecosystem) -> Index {
    Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem,
        kind: IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    }
}

async fn get(app: &Router, uri: &str) -> StatusCode {
    app.clone()
        .oneshot(HttpRequest::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

fn settings() -> HashMap<String, CompiledEcosystemSettings> {
    HashMap::from([
        ("primary-b".to_owned(), CompiledEcosystemSettings::new(PRIMARY, ())),
        ("primary-a".to_owned(), CompiledEcosystemSettings::new(PRIMARY, ())),
        ("secondary".to_owned(), CompiledEcosystemSettings::new(SECONDARY, ())),
    ])
}
