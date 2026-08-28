use std::cell::Cell;
use std::sync::Arc;

use crate::{OperatorJob, OperatorJobDefaults, OperatorJobOptions, PluginAuthRegistration, PluginRegistration};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use peryx_core::{DefaultIndex, DefaultIndexKind, Ecosystem};
use peryx_driver::AppState;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{
    AbsoluteProtocolDriver, AuthInstallContext, CapabilityRegistrar, ClientDiscovery, CompiledEcosystemSettings,
    DistributedInstallContext, DistributedRuntime, EcosystemAuth, EcosystemBrowse, EcosystemConfig, EcosystemDriver,
    EcosystemOpenApi, EcosystemRegistration, EcosystemRuntime, EcosystemSnippet, JobConfig, JobDriver,
    PluginAuthConfig, ProtocolDriver, RateLimitPrincipal, RuntimeInstallContext,
};
use peryx_driver::state::{IndexDescription, ServingState};
use utoipa::openapi::PathsBuilder;
use utoipa::openapi::path::{HttpMethod, Operation, PathItem};

pub const PRIMARY: Ecosystem = Ecosystem::new("alpha");
pub const SECONDARY: Ecosystem = Ecosystem::new("beta");

thread_local! {
    static DRIVER_FACTORY_CALLS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

pub fn reset_driver_factory_calls() {
    DRIVER_FACTORY_CALLS.set((0, 0));
}

pub fn driver_factory_calls() -> (usize, usize) {
    DRIVER_FACTORY_CALLS.get()
}

pub fn registrations() -> Vec<PluginRegistration> {
    vec![
        PluginRegistration {
            registration: &SECONDARY_REGISTRATION,
            config: &SECONDARY_CONFIG,
            runtime: &RUNTIME,
            distributed_runtime: Some(&RUNTIME),
            rate_limit_principal: Some(&SECONDARY_REGISTRATION),
            client_discovery: Some(&SECONDARY_REGISTRATION),
            openapi: &SECONDARY_OPEN_API,
            auth: Some(PluginAuthRegistration::Extension {
                auth: &SECONDARY_AUTH,
                fields: &["secondary"],
                defaults: secondary_auth_defaults,
            }),
            browse: Some(&SECONDARY_BROWSE),
            snippets: Some(&SNIPPETS),
            metadata_migration: None,
            operator_jobs: &SECONDARY_OPERATOR_JOBS,
            priority: 10,
        },
        PluginRegistration {
            registration: &PRIMARY_REGISTRATION,
            config: &PRIMARY_CONFIG,
            runtime: &RUNTIME,
            distributed_runtime: Some(&RUNTIME),
            rate_limit_principal: Some(&PRIMARY_REGISTRATION),
            client_discovery: Some(&PRIMARY_REGISTRATION),
            openapi: &PRIMARY_OPEN_API,
            auth: Some(PluginAuthRegistration::Extension {
                auth: &PRIMARY_AUTH,
                fields: &["primary"],
                defaults: primary_auth_defaults,
            }),
            browse: Some(&PRIMARY_BROWSE),
            snippets: Some(&SNIPPETS),
            metadata_migration: None,
            operator_jobs: &PRIMARY_OPERATOR_JOBS,
            priority: 20,
        },
    ]
}

static PRIMARY_OPERATOR_JOB: TestOperatorJob = TestOperatorJob {
    command: "run",
    ecosystem: PRIMARY,
    defaults: OperatorJobDefaults {
        item_limit: 10,
        concurrency: 2,
        timeout_secs: 30,
    },
};
static PRIMARY_OPERATOR_JOBS: [&dyn OperatorJob; 1] = [&PRIMARY_OPERATOR_JOB];
static SECONDARY_OPERATOR_JOB: TestOperatorJob = TestOperatorJob {
    command: "sync",
    ecosystem: SECONDARY,
    defaults: OperatorJobDefaults {
        item_limit: 20,
        concurrency: 4,
        timeout_secs: 60,
    },
};
static SECONDARY_OPERATOR_JOBS: [&dyn OperatorJob; 1] = [&SECONDARY_OPERATOR_JOB];

struct TestOperatorJob {
    command: &'static str,
    ecosystem: Ecosystem,
    defaults: OperatorJobDefaults,
}

impl OperatorJob for TestOperatorJob {
    fn command(&self) -> &'static str {
        self.command
    }

    fn defaults(&self) -> OperatorJobDefaults {
        self.defaults
    }

    fn compile(&self, options: OperatorJobOptions<'_>) -> Result<peryx_driver::jobs::PluginScheduledJob, String> {
        if options.target.is_empty() {
            return Err(format!("{} target is empty", self.command));
        }
        Ok(peryx_driver::jobs::PluginScheduledJob::new(
            self.ecosystem.clone(),
            Arc::new(TestJobFactory {
                kind: self.command,
                settings: toml::Table::from_iter([
                    ("target".to_owned(), toml::Value::String(options.target.to_owned())),
                    (
                        "source".to_owned(),
                        toml::Value::String(options.source.unwrap_or_default().to_owned()),
                    ),
                    (
                        "item-limit".to_owned(),
                        toml::Value::String(options.item_limit.to_string()),
                    ),
                    (
                        "concurrency".to_owned(),
                        toml::Value::String(options.concurrency.to_string()),
                    ),
                    (
                        "timeout-secs".to_owned(),
                        toml::Value::String(options.timeout_secs.to_string()),
                    ),
                ]),
            }),
        ))
    }
}

pub static PRIMARY_REGISTRATION: Registration = Registration {
    ecosystem: PRIMARY,
    driver_ecosystem: PRIMARY,
    prefixes: &[],
    jobs: true,
};
pub static SECONDARY_REGISTRATION: Registration = Registration {
    ecosystem: SECONDARY,
    driver_ecosystem: SECONDARY,
    prefixes: &[],
    jobs: true,
};
pub static MISMATCHED_REGISTRATION: Registration = Registration {
    ecosystem: PRIMARY,
    driver_ecosystem: SECONDARY,
    prefixes: &[],
    jobs: true,
};
pub static NO_JOBS_REGISTRATION: Registration = Registration {
    ecosystem: PRIMARY,
    driver_ecosystem: PRIMARY,
    prefixes: &[],
    jobs: false,
};
pub static PRIMARY_V2_REGISTRATION: Registration = Registration {
    ecosystem: PRIMARY,
    driver_ecosystem: PRIMARY,
    prefixes: &["/v2/"],
    jobs: true,
};
pub static PRIMARY_V2_UPLOADS_REGISTRATION: Registration = Registration {
    ecosystem: PRIMARY,
    driver_ecosystem: PRIMARY,
    prefixes: &["/v2/uploads/"],
    jobs: true,
};
pub static SECONDARY_V2_REGISTRATION: Registration = Registration {
    ecosystem: SECONDARY,
    driver_ecosystem: SECONDARY,
    prefixes: &["/v2/"],
    jobs: true,
};
pub static SECONDARY_V2_UPLOADS_REGISTRATION: Registration = Registration {
    ecosystem: SECONDARY,
    driver_ecosystem: SECONDARY,
    prefixes: &["/v2/uploads/"],
    jobs: true,
};
pub static SECONDARY_V20_REGISTRATION: Registration = Registration {
    ecosystem: SECONDARY,
    driver_ecosystem: SECONDARY,
    prefixes: &["/v20/"],
    jobs: true,
};
static PRIMARY_DEFAULT_INDEXES: [DefaultIndex; 1] = [DefaultIndex {
    name: "default",
    route: "default",
    ecosystem: PRIMARY,
    kind: DefaultIndexKind::Hosted,
}];
static SECONDARY_DEFAULT_INDEXES: [DefaultIndex; 1] = [DefaultIndex {
    name: "default",
    route: "default",
    ecosystem: SECONDARY,
    kind: DefaultIndexKind::Hosted,
}];
pub static PRIMARY_AUTH: Auth = Auth(PRIMARY);
pub static SECONDARY_AUTH: Auth = Auth(SECONDARY);
static PRIMARY_CONFIG: Config = Config(PRIMARY);
static SECONDARY_CONFIG: Config = Config(SECONDARY);
static RUNTIME: Runtime = Runtime;
static PRIMARY_BROWSE: Browse = Browse(PRIMARY);
static SECONDARY_BROWSE: Browse = Browse(SECONDARY);
static PRIMARY_OPEN_API: OpenApi = OpenApi(PRIMARY);
static SECONDARY_OPEN_API: OpenApi = OpenApi(SECONDARY);
static SNIPPETS: Snippets = Snippets;

pub struct Registration {
    ecosystem: Ecosystem,
    driver_ecosystem: Ecosystem,
    prefixes: &'static [&'static str],
    jobs: bool,
}

impl EcosystemRegistration for Registration {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem.clone()
    }

    fn default_indexes(&self) -> &'static [DefaultIndex] {
        if self.ecosystem == PRIMARY {
            &PRIMARY_DEFAULT_INDEXES
        } else {
            &SECONDARY_DEFAULT_INDEXES
        }
    }

    fn absolute_prefixes(&self) -> &'static [&'static str] {
        self.prefixes
    }

    fn driver(&self) -> ProtocolDriver {
        DRIVER_FACTORY_CALLS.with(|calls| {
            let (primary, secondary) = calls.get();
            if self.ecosystem == PRIMARY {
                calls.set((primary + 1, secondary));
            } else {
                calls.set((primary, secondary + 1));
            }
        });
        ProtocolDriver::Absolute(Arc::new(Driver {
            ecosystem: self.driver_ecosystem.clone(),
            prefixes: self.prefixes,
        }))
    }

    fn register_capabilities(&self, registrar: &mut dyn CapabilityRegistrar) {
        if self.jobs {
            registrar.register_job(
                self.ecosystem.clone(),
                Arc::new(Driver {
                    ecosystem: self.driver_ecosystem.clone(),
                    prefixes: self.prefixes,
                }),
            );
        }
    }
}

struct Config(Ecosystem);

impl EcosystemConfig for Config {
    fn compile_index_settings(&self, name: &str, _: &toml::Table) -> Result<Option<CompiledEcosystemSettings>, String> {
        match name {
            "" => Err("index name is empty".to_owned()),
            "optional" => Ok(None),
            _ => Ok(Some(CompiledEcosystemSettings::new(self.0.clone(), name.to_owned()))),
        }
    }
}

struct Runtime;

impl EcosystemRuntime for Runtime {
    fn install(
        &self,
        context: &mut RuntimeInstallContext<'_>,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        install(context, settings, "local")
    }
}

impl DistributedRuntime for Runtime {
    fn install(
        &self,
        context: &mut DistributedInstallContext<'_>,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        install(context.runtime(), settings, "distributed")
    }
}

impl ClientDiscovery for Registration {
    fn discover_index(&self, index: IndexDescription, _: Option<&BaseUrl>) -> serde_json::Value {
        serde_json::Value::String(index.name)
    }

    fn client_endpoint(&self, route: &str) -> String {
        format!("/{route}/")
    }
}

impl RateLimitPrincipal for Registration {
    fn resolve(
        &self,
        _state: &ServingState,
        _position: Option<usize>,
        _headers: &axum::http::HeaderMap,
    ) -> peryx_identity::Principal {
        peryx_identity::Principal::Named {
            subject: self.ecosystem.as_str().to_owned(),
        }
    }
}

pub struct Auth(Ecosystem);

#[derive(Debug, Eq, PartialEq)]
pub(super) struct AuthInstallMarker(pub(super) Ecosystem);

impl EcosystemAuth for Auth {
    fn validate(&self, config: PluginAuthConfig<'_>) -> Result<(), String> {
        if config
            .values
            .values()
            .any(|value| value == &toml::Value::Boolean(false))
        {
            Err(format!("{} auth rejected", self.0))
        } else {
            Ok(())
        }
    }

    fn install(&self, context: &mut AuthInstallContext<'_>, values: &toml::Table) -> Result<(), String> {
        if values.values().any(|value| value == &toml::Value::Boolean(false)) {
            Err(format!("{} auth install failed", self.0))
        } else {
            context.register_service(Arc::new(AuthInstallMarker(self.0.clone())));
            Ok(())
        }
    }
}

fn primary_auth_defaults() -> toml::Table {
    toml::Table::from_iter([("primary".to_owned(), toml::Value::Boolean(true))])
}

fn secondary_auth_defaults() -> toml::Table {
    toml::Table::from_iter([("secondary".to_owned(), toml::Value::Boolean(true))])
}

struct Browse(Ecosystem);

#[async_trait::async_trait]
impl EcosystemBrowse for Browse {
    fn paths(&self) -> &'static [&'static str] {
        if self.0 == PRIMARY {
            &["/browse/shared", "/browse/alpha"]
        } else {
            &["/browse/shared", "/browse/beta"]
        }
    }

    async fn dispatch(&self, _: Arc<AppState>, _: Request) -> Response {
        if self.0 == PRIMARY {
            StatusCode::ACCEPTED.into_response()
        } else {
            StatusCode::CREATED.into_response()
        }
    }
}

struct OpenApi(Ecosystem);

impl EcosystemOpenApi for OpenApi {
    fn paths(&self, paths: PathsBuilder) -> PathsBuilder {
        paths.path(
            if self.0 == PRIMARY {
                "/alpha-extension"
            } else {
                "/beta-extension"
            },
            PathItem::new(HttpMethod::Get, Operation::new()),
        )
    }
}

struct Snippets;

impl EcosystemSnippet for Snippets {
    fn text(&self, base: &BaseUrl, route: &str, uploads: bool, format: &str) -> Result<Option<String>, String> {
        if format.is_empty() {
            Err("format is empty".to_owned())
        } else {
            Ok(Some(base.join(&format!("/{route}?uploads={uploads}&format={format}"))))
        }
    }
}

struct Driver {
    ecosystem: Ecosystem,
    prefixes: &'static [&'static str],
}

impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem.clone()
    }
}

impl JobDriver for Driver {
    fn compile_job(&self, config: JobConfig<'_>) -> Option<Result<peryx_driver::jobs::PluginScheduledJob, String>> {
        let ecosystem = match config.kind {
            "shared_job" if self.ecosystem == PRIMARY => self.ecosystem.clone(),
            "duplicate_job" => self.ecosystem.clone(),
            "foreign_job" if self.ecosystem == PRIMARY => SECONDARY,
            _ => return None,
        };
        Some(Ok(peryx_driver::jobs::PluginScheduledJob::new(
            ecosystem,
            Arc::new(TestJobFactory {
                kind: "shared_job",
                settings: toml::Table::new(),
            }),
        )))
    }
}

#[async_trait::async_trait]
impl AbsoluteProtocolDriver for Driver {
    fn prefixes(&self) -> &'static [&'static str] {
        self.prefixes
    }

    fn classify_route(&self, _: &str) -> RouteClass {
        RouteClass::Metadata
    }

    async fn serve(&self, _: Arc<ServingState>, _: Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

pub struct TestJobFactory {
    pub kind: &'static str,
    pub settings: toml::Table,
}

impl peryx_driver::jobs::ScheduledJobFactory for TestJobFactory {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn settings(&self) -> toml::Table {
        self.settings.clone()
    }

    fn create(&self, _: &AppState) -> Result<Arc<dyn peryx_driver::jobs::NodeJob>, String> {
        Err("test factory does not execute".to_owned())
    }
}

fn install(
    context: &mut RuntimeInstallContext<'_>,
    settings: &[(&str, &CompiledEcosystemSettings)],
    mode: &'static str,
) -> Result<(), String> {
    if settings.iter().any(|(name, _)| name.is_empty()) {
        return Err(format!("{mode} install failed"));
    }
    context.register_service(Arc::new(RuntimeInstallMarker {
        mode,
        settings: settings.iter().map(|(name, _)| (*name).to_owned()).collect(),
    }));
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct RuntimeInstallMarker {
    pub(super) mode: &'static str,
    pub(super) settings: Vec<String>,
}
