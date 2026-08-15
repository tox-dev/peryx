use std::io::Cursor;
use std::sync::Arc;

use peryx_core::{DefaultIndex, DefaultIndexKind, Ecosystem};
use peryx_driver::AppState;
use peryx_driver::jobs::{
    JobContext, JobFailure, JobReport, LeaseScope, NodeJob, NodeJobMetadata, PluginScheduledJob, ScheduledJobFactory,
};
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{
    CompiledEcosystemSettings, DistributedInstallContext, EcosystemConfig, EcosystemOpenApi, EcosystemRegistration,
    EcosystemRuntime, ProtocolDriver, RuntimeInstallContext,
};
use peryx_plugin_registry::{
    OperatorJob, OperatorJobDefaults, OperatorJobOptions, OperatorJobRequest, PluginRegistration, PluginRegistry,
};
use peryx_storage::meta::{
    IntentAdmission, IntentLimits, IntentPhase, JobKind, JobOutcome, JobState, MetaStore, NewJobRun,
};
use peryx_test_support::EcosystemDriverFixture;
use rstest::rstest;
use utoipa::openapi::PathsBuilder;

use crate::app::{job, job_with_plugins};
use crate::cli::{JobCommand, JobListArgs, JobShowArgs, RuntimeArgs};
use crate::config::{AvailabilityConfig, Config, ReplicationConfig, SecretSource};

const CORE: Ecosystem = Ecosystem::new("core");
const INACTIVE: Ecosystem = Ecosystem::new("inactive");

#[test]
fn test_job_public_entrypoint_reports_a_missing_store() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: directory.path().join("missing"),
        ..Config::default()
    };

    let error = job(&config, &list_command(), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("open metadata store"), "{error:#}");
}

#[test]
fn test_job_lists_durable_runs() {
    let plugins = plugins();
    let (_directory, config, running, failed) = config_with_runs(&plugins);
    let mut output = Vec::new();

    job_with_plugins(&config, &plugins, &list_command(), &mut output).unwrap();

    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
        serde_json::json!({
            "attempts": [
                {
                    "id": failed,
                    "kind": "maintenance",
                    "scope": "group/main",
                    "repository": null,
                    "state": "failed",
                    "started_at_unix": 20,
                    "finished_at_unix": 21,
                    "items_processed": 4,
                    "items_changed": 1,
                    "error": "source unavailable"
                },
                {
                    "id": running,
                    "kind": "maintenance",
                    "scope": "main",
                    "repository": null,
                    "state": "running",
                    "started_at_unix": 10,
                    "finished_at_unix": null,
                    "items_processed": 0,
                    "items_changed": 0,
                    "error": null
                }
            ],
            "next_cursor": null
        })
    );
}

#[test]
fn test_job_shows_a_durable_run() {
    let plugins = plugins();
    let (_directory, config, _running, failed) = config_with_runs(&plugins);
    let mut output = Vec::new();

    job_with_plugins(&config, &plugins, &show_command(&failed), &mut output).unwrap();

    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
        serde_json::json!({
            "id": failed,
            "kind": "maintenance",
            "scope": "group/main",
            "repository": null,
            "state": "failed",
            "started_at_unix": 20,
            "finished_at_unix": 21,
            "items_processed": 4,
            "items_changed": 1,
            "error": "source unavailable"
        })
    );
}

#[test]
fn test_job_show_rejects_an_unknown_run() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);

    let error = job_with_plugins(&config, &plugins, &show_command("missing"), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "unknown job run \"missing\"");
}

#[test]
fn test_job_list_reports_an_output_failure() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);

    let error = job_with_plugins(&config, &plugins, &list_command(), &mut bounded_output(0)).unwrap_err();

    assert!(error.to_string().contains("failed to write whole buffer"), "{error:#}");
}

#[test]
fn test_job_list_reports_a_row_output_failure() {
    let plugins = plugins();
    let (_directory, config, _, _) = config_with_runs(&plugins);
    let mut complete = Vec::new();
    job_with_plugins(&config, &plugins, &list_command(), &mut complete).unwrap();

    let error = job_with_plugins(
        &config,
        &plugins,
        &list_command(),
        &mut bounded_before(&complete, "maintenance"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("failed to write whole buffer"), "{error:#}");
}

#[test]
fn test_job_show_reports_an_output_failure() {
    let plugins = plugins();
    let (_directory, config, _, failed) = config_with_runs(&plugins);

    let error = job_with_plugins(&config, &plugins, &show_command(&failed), &mut bounded_output(0)).unwrap_err();

    assert!(error.to_string().contains("failed to write whole buffer"), "{error:#}");
}

#[test]
fn test_job_reports_the_missing_store_path() {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: directory.path().join("missing"),
        ..Config::with_plugins(&plugins)
    };

    let error = job_with_plugins(&config, &plugins, &list_command(), &mut Vec::new()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "open metadata store {} read-only",
            config.data_dir.join("peryx.redb").display()
        )
    );
}

#[test]
fn test_job_run_reports_the_registered_job_counts() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let mut output = Vec::new();

    job_with_plugins(&config, &plugins, &run_command(), &mut output).unwrap();

    assert_eq!(String::from_utf8(output).unwrap(), "processed\t2\nchanged\t1\n");
}

#[test]
fn test_job_run_records_the_registered_job() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);

    job_with_plugins(&config, &plugins, &run_command(), &mut Vec::new()).unwrap();

    let runs = MetaStore::open_existing(config.data_dir.join("peryx.redb"))
        .unwrap()
        .list_job_runs()
        .unwrap();
    assert_eq!(
        (
            runs[0].kind.clone(),
            runs[0].scope.as_str(),
            runs[0].state,
            runs[0].items_processed,
            runs[0].items_changed,
        ),
        (JobKind::new("core_sync").unwrap(), "main", JobState::Succeeded, 2, 1,)
    );
}

#[test]
fn test_job_run_does_not_select_an_inactive_owner() {
    let plugins = plugins_with_inactive_job();
    let directory = tempfile::tempdir().unwrap();
    let config = config_at(&directory, &plugins);

    let error = job_with_plugins(&config, &plugins, &run_command(), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "unknown operator job command \"run\"");
}

#[test]
fn test_job_registry_rejects_the_unsupported_distributed_runtime() {
    let plugins = plugins_with_distributed_runtime(&REJECTING_RUNTIME);
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let state = crate::server::build_state_with_plugins(&config, &plugins).unwrap();
    let mut state = Arc::into_inner(state).expect("newly built state has no other owners");

    let error = plugins
        .install_distributed_drivers(
            &mut state.distributed_install_context().unwrap(),
            &std::collections::HashMap::new(),
        )
        .unwrap_err();

    assert_eq!(error, "job fixture has no distributed runtime");
}

#[test]
fn test_job_registry_compiles_scheduled_job_metadata() {
    let scheduled = plugins()
        .compile_operator_job(
            "run",
            OperatorJobRequest {
                target: "main",
                source: None,
                item_limit: Some(2),
                concurrency: Some(1),
                timeout_secs: Some(30),
            },
        )
        .unwrap();

    assert_eq!(
        (scheduled.kind(), scheduled.settings()),
        ("core_sync", toml::Table::new())
    );
}

#[test]
fn test_job_reindex_records_a_node_wide_run() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);

    job_with_plugins(&config, &plugins, &reindex_command(1), &mut Vec::new()).unwrap();

    let runs = MetaStore::open_existing(config.data_dir.join("peryx.redb"))
        .unwrap()
        .list_job_runs()
        .unwrap();
    assert_eq!(
        (runs[0].kind.clone(), runs[0].scope.as_str(), runs[0].state),
        (JobKind::new("search_rebuild").unwrap(), "", JobState::Succeeded,)
    );
}

#[rstest]
#[case::zero(0, "chunk-size must be positive")]
#[case::above_limit(peryx_driver::jobs::MAX_SEARCH_REBUILD_CHUNK + 1, "chunk-size exceeds the per-run limit")]
fn test_job_reindex_rejects_invalid_chunks(#[case] chunk_size: usize, #[case] expected: &str) {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();
    let config = config_at(&directory, &plugins);

    let error = job_with_plugins(&config, &plugins, &reindex_command(chunk_size), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), expected);
}

#[test]
fn test_job_drain_finalizes_retained_intents() {
    let plugins = plugins();
    let (_directory, config) = config_with_intents(&plugins);

    job_with_plugins(&config, &plugins, &drain_command(), &mut Vec::new()).unwrap();

    let meta = MetaStore::open_existing(config.data_dir.join("peryx.redb")).unwrap();
    assert_eq!(
        (
            meta.staged_intent("group\0resource\0key-1").unwrap().unwrap().phase,
            meta.staged_intent("group\0resource\0key-2").unwrap().unwrap().phase,
            meta.list_pending_intents(10).unwrap(),
        ),
        (IntentPhase::Admitted, IntentPhase::Admitted, Vec::new())
    );
}

#[test]
fn test_job_drain_reports_counts() {
    let plugins = plugins();
    let (_directory, config) = config_with_intents(&plugins);
    let mut output = Vec::new();

    job_with_plugins(&config, &plugins, &drain_command(), &mut output).unwrap();

    assert_eq!(String::from_utf8(output).unwrap(), "processed\t2\nchanged\t2\n");
}

#[test]
fn test_job_drain_records_the_authority() {
    let plugins = plugins();
    let (_directory, config) = config_with_intents(&plugins);

    job_with_plugins(&config, &plugins, &drain_command(), &mut Vec::new()).unwrap();

    let runs = MetaStore::open_existing(config.data_dir.join("peryx.redb"))
        .unwrap()
        .list_job_runs()
        .unwrap();
    assert_eq!(
        (runs[0].kind.clone(), runs[0].repository.as_deref(), runs[0].state,),
        (
            JobKind::new("authority_drain").unwrap(),
            Some("group/resource"),
            JobState::Succeeded,
        )
    );
}

#[test]
fn test_job_drain_rejects_an_empty_authority() {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();
    let config = config_at(&directory, &plugins);

    let error = job_with_plugins(&config, &plugins, &drain_command_for("  "), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "authority must not be empty");
}

#[test]
fn test_job_drain_rejects_none_before_opening_metadata() {
    let plugins = plugins();
    let directory = tempfile::tempdir().unwrap();
    let config = config_at(&directory, &plugins);

    let error = job_with_plugins(&config, &plugins, &drain_command(), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "authority drain requires distributed availability");
    assert!(!config.data_dir.join("peryx.redb").exists());
}

fn config_with_runs(plugins: &PluginRegistry) -> (tempfile::TempDir, Config, String, String) {
    let (directory, meta, config) = store_and_config(plugins);
    let running = start_job(&meta, "main", 10);
    let failed = start_job(&meta, "group/main", 20);
    meta.finish_job_run(&failed, JobOutcome::failed(21, 4, 1, "source unavailable"))
        .unwrap();
    drop(meta);
    (directory, config, running, failed)
}

fn config_with_intents(plugins: &PluginRegistry) -> (tempfile::TempDir, Config) {
    let (directory, meta, mut config) = store_and_config(plugins);
    config.availability = AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "local".to_owned(),
        token: SecretSource::Literal("token".to_owned()),
    });
    for (key, digest, payload) in [
        ("group\0resource\0key-1", "digest-a", b"one".as_slice()),
        ("group\0resource\0key-2", "digest-b", b"two".as_slice()),
    ] {
        meta.stage_intent(
            IntentAdmission {
                authority: "group/resource",
                key,
                digest,
                size: payload.len().try_into().unwrap(),
                payload,
            },
            IntentLimits {
                max_records: 10,
                max_bytes: 1 << 20,
                backpressure_percent: 80,
            },
            1,
        )
        .unwrap();
    }
    drop(meta);
    (directory, config)
}

fn store_and_config(plugins: &PluginRegistry) -> (tempfile::TempDir, MetaStore, Config) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let config = config_at(&directory, plugins);
    (directory, meta, config)
}

fn config_at(directory: &tempfile::TempDir, plugins: &PluginRegistry) -> Config {
    Config {
        data_dir: directory.path().to_path_buf(),
        ..Config::with_plugins(plugins)
    }
}

fn start_job(meta: &MetaStore, scope: &str, started_at_unix: i64) -> String {
    meta.start_job_run(NewJobRun {
        kind: JobKind::new("maintenance").unwrap(),
        scope,
        repository: None,
        started_at_unix,
    })
    .unwrap()
}

fn list_command() -> JobCommand {
    JobCommand::List(JobListArgs {
        runtime: RuntimeArgs::default(),
    })
}

fn show_command(id: &str) -> JobCommand {
    JobCommand::Show(JobShowArgs {
        runtime: RuntimeArgs::default(),
        id: id.to_owned(),
    })
}

fn run_command() -> JobCommand {
    JobCommand::Run {
        runtime: RuntimeArgs::default(),
        target: "main".to_owned(),
        source: None,
        item_limit: Some(2),
        concurrency: Some(1),
        timeout_secs: Some(30),
    }
}

fn reindex_command(chunk_size: usize) -> JobCommand {
    JobCommand::Reindex {
        runtime: RuntimeArgs::default(),
        chunk_size,
    }
}

fn drain_command() -> JobCommand {
    drain_command_for("group/resource")
}

fn drain_command_for(authority: &str) -> JobCommand {
    JobCommand::Drain {
        runtime: RuntimeArgs::default(),
        authority: authority.to_owned(),
    }
}

fn plugins() -> PluginRegistry {
    plugins_with_distributed_runtime(&RUNTIME)
}

fn plugins_with_distributed_runtime(
    distributed_runtime: &'static dyn peryx_driver::serving::DistributedRuntime,
) -> PluginRegistry {
    PluginRegistry::new(vec![PluginRegistration {
        registration: &REGISTRATION,
        config: &ECOSYSTEM_CONFIG,
        runtime: &RUNTIME,
        distributed_runtime: Some(distributed_runtime),
        rate_limit_principal: None,
        client_discovery: None,
        openapi: &OPEN_API,
        auth: None,
        browse: None,
        snippets: None,
        metadata_migration: None,
        operator_jobs: &OPERATOR_JOBS,
        priority: 1,
    }])
    .unwrap()
}

fn plugins_with_inactive_job() -> PluginRegistry {
    PluginRegistry::new(vec![
        PluginRegistration {
            registration: &REGISTRATION,
            config: &ECOSYSTEM_CONFIG,
            runtime: &RUNTIME,
            distributed_runtime: Some(&RUNTIME),
            rate_limit_principal: None,
            client_discovery: None,
            openapi: &OPEN_API,
            auth: None,
            browse: None,
            snippets: None,
            metadata_migration: None,
            operator_jobs: &[],
            priority: 1,
        },
        PluginRegistration {
            registration: &INACTIVE_REGISTRATION,
            config: &ECOSYSTEM_CONFIG,
            runtime: &RUNTIME,
            distributed_runtime: Some(&RUNTIME),
            rate_limit_principal: None,
            client_discovery: None,
            openapi: &OPEN_API,
            auth: None,
            browse: None,
            snippets: None,
            metadata_migration: None,
            operator_jobs: &OPERATOR_JOBS,
            priority: 2,
        },
    ])
    .unwrap()
}

static REGISTRATION: Registration = Registration {
    ecosystem: CORE,
    driver: &CORE_DRIVER,
    default_indexes: &[DefaultIndex {
        name: "main",
        route: "main",
        ecosystem: CORE,
        kind: DefaultIndexKind::Hosted,
    }],
};
static INACTIVE_REGISTRATION: Registration = Registration {
    ecosystem: INACTIVE,
    driver: &INACTIVE_DRIVER,
    default_indexes: &[],
};
static CORE_DRIVER: EcosystemDriverFixture = EcosystemDriverFixture::new(CORE, RouteClass::Metadata);
static INACTIVE_DRIVER: EcosystemDriverFixture = EcosystemDriverFixture::new(INACTIVE, RouteClass::Metadata);
static ECOSYSTEM_CONFIG: EcosystemConfigImpl = EcosystemConfigImpl;
static RUNTIME: Runtime = Runtime;
static REJECTING_RUNTIME: RejectingRuntime = RejectingRuntime;
static OPEN_API: OpenApi = OpenApi;
static OPERATOR_JOB: RunOperatorJob = RunOperatorJob;
static OPERATOR_JOBS: [&dyn OperatorJob; 1] = [&OPERATOR_JOB];

struct Registration {
    ecosystem: Ecosystem,
    driver: &'static EcosystemDriverFixture,
    default_indexes: &'static [DefaultIndex],
}

impl EcosystemRegistration for Registration {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem.clone()
    }

    fn default_indexes(&self) -> &'static [DefaultIndex] {
        self.default_indexes
    }

    fn driver(&self) -> ProtocolDriver {
        ProtocolDriver::Absolute(Arc::new((*self.driver).clone()))
    }
}

fn bounded_output(capacity: usize) -> Cursor<Box<[u8]>> {
    Cursor::new(vec![0; capacity].into_boxed_slice())
}

fn bounded_before(output: &[u8], needle: &str) -> Cursor<Box<[u8]>> {
    bounded_output(
        output
            .windows(needle.len())
            .position(|window| window == needle.as_bytes())
            .unwrap(),
    )
}

struct EcosystemConfigImpl;

impl EcosystemConfig for EcosystemConfigImpl {
    fn compile_index_settings(&self, _: &str, _: &toml::Table) -> Result<Option<CompiledEcosystemSettings>, String> {
        Ok(None)
    }
}

struct Runtime;

impl EcosystemRuntime for Runtime {
    fn install(
        &self,
        _: &mut RuntimeInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        Ok(())
    }
}

impl peryx_driver::serving::DistributedRuntime for Runtime {
    fn install(
        &self,
        _: &mut DistributedInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        Ok(())
    }
}

struct RejectingRuntime;

impl peryx_driver::serving::DistributedRuntime for RejectingRuntime {
    fn install(
        &self,
        _: &mut DistributedInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        Err("job fixture has no distributed runtime".to_owned())
    }
}

struct OpenApi;

impl EcosystemOpenApi for OpenApi {
    fn paths(&self, paths: PathsBuilder) -> PathsBuilder {
        paths
    }
}

struct RunOperatorJob;

impl OperatorJob for RunOperatorJob {
    fn command(&self) -> &'static str {
        "run"
    }

    fn defaults(&self) -> OperatorJobDefaults {
        OperatorJobDefaults {
            item_limit: 4,
            concurrency: 3,
            timeout_secs: 30,
        }
    }

    fn compile(&self, options: OperatorJobOptions<'_>) -> Result<PluginScheduledJob, String> {
        Ok(PluginScheduledJob::new(
            CORE,
            Arc::new(RunJobFactory {
                target: options.target.to_owned(),
                processed: u64::try_from(options.item_limit).expect("usize fits in u64"),
                changed: u64::try_from(options.concurrency).expect("usize fits in u64"),
            }),
        ))
    }
}

struct RunJobFactory {
    target: String,
    processed: u64,
    changed: u64,
}

impl ScheduledJobFactory for RunJobFactory {
    fn kind(&self) -> &'static str {
        "core_sync"
    }

    fn settings(&self) -> toml::Table {
        toml::Table::new()
    }

    fn create(&self, _: &AppState) -> Result<Arc<dyn NodeJob>, String> {
        Ok(Arc::new(RunJob {
            target: self.target.clone(),
            report: JobReport {
                processed: self.processed,
                changed: self.changed,
            },
        }))
    }
}

struct RunJob {
    target: String,
    report: JobReport,
}

#[async_trait::async_trait]
impl NodeJob for RunJob {
    fn kind(&self) -> &'static str {
        "core_sync"
    }

    fn scope(&self) -> &str {
        &self.target
    }

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: Some(JobKind::new("core_sync").unwrap()),
        }
    }

    async fn run(&self, _: &JobContext) -> Result<JobReport, JobFailure> {
        Ok(self.report)
    }
}
