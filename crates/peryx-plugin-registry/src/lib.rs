//! Builds capability registries from compiled registrations and activates only ecosystems selected by configuration.

use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, Request, State};
use axum::response::Response;
use axum::routing::get;
use peryx_core::Ecosystem;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{
    CapabilityInstallContext, ClientDiscovery, CompiledEcosystemSettings, DistributedInstallContext,
    DistributedRuntime, EcosystemAuth, EcosystemBrowse, EcosystemConfig, EcosystemDriver, EcosystemOpenApi,
    EcosystemRegistration, EcosystemRuntime, EcosystemSnippet, JobConfig, PluginAuthConfig, PluginIndexConfig,
    ProtocolDriver, RateLimitPrincipal, RuntimeInstallContext,
};
use peryx_driver::{AppState, DriverSet, HttpRoutes};
use utoipa::openapi::PathsBuilder;

#[cfg(test)]
#[path = "../tests/unit.rs"]
mod tests;

#[derive(Clone)]
pub struct PluginRegistration {
    pub registration: &'static dyn EcosystemRegistration,
    pub config: &'static dyn EcosystemConfig,
    pub runtime: &'static dyn EcosystemRuntime,
    pub distributed_runtime: Option<&'static dyn DistributedRuntime>,
    pub rate_limit_principal: Option<&'static dyn RateLimitPrincipal>,
    pub client_discovery: Option<&'static dyn ClientDiscovery>,
    pub openapi: &'static dyn EcosystemOpenApi,
    pub auth: Option<&'static dyn EcosystemAuth>,
    pub browse: Option<&'static dyn EcosystemBrowse>,
    pub snippets: Option<&'static dyn EcosystemSnippet>,
    pub metadata_migration: Option<Arc<dyn peryx_storage::meta::MetadataMigration>>,
    pub operator_jobs: &'static [&'static dyn OperatorJob],
    pub priority: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorJobDefaults {
    pub item_limit: usize,
    pub concurrency: usize,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorJobOptions<'a> {
    pub target: &'a str,
    pub source: Option<&'a str>,
    pub item_limit: usize,
    pub concurrency: usize,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorJobRequest<'a> {
    pub target: &'a str,
    pub source: Option<&'a str>,
    pub item_limit: Option<usize>,
    pub concurrency: Option<usize>,
    pub timeout_secs: Option<u64>,
}

pub trait OperatorJob: Send + Sync {
    fn command(&self) -> &'static str;

    fn defaults(&self) -> OperatorJobDefaults;

    /// # Errors
    /// Returns a stable configuration error when the plugin cannot compile the requested job.
    fn compile(&self, options: OperatorJobOptions<'_>) -> Result<peryx_driver::jobs::PluginScheduledJob, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    Empty,
    MissingEcosystem(Ecosystem),
    InactiveEcosystem(Ecosystem),
    DuplicateEcosystem(Ecosystem),
    DuplicatePriority(u16),
    DuplicateOperatorJob(&'static str),
    DuplicateAuthField(&'static str),
    AbsolutePrefixConflict {
        first_ecosystem: Ecosystem,
        first_prefix: &'static str,
        second_ecosystem: Ecosystem,
        second_prefix: &'static str,
    },
    DriverEcosystem {
        registration: Ecosystem,
        driver: Ecosystem,
    },
}

impl Display for RegistryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("at least one ecosystem registration is required"),
            Self::MissingEcosystem(ecosystem) => write!(formatter, "ecosystem {ecosystem} is not installed"),
            Self::InactiveEcosystem(ecosystem) => write!(formatter, "ecosystem {ecosystem} is not active"),
            Self::DuplicateEcosystem(ecosystem) => write!(formatter, "duplicate ecosystem {ecosystem}"),
            Self::DuplicatePriority(priority) => write!(formatter, "duplicate ecosystem priority {priority}"),
            Self::DuplicateOperatorJob(command) => write!(formatter, "duplicate operator job command {command:?}"),
            Self::DuplicateAuthField(field) => write!(formatter, "duplicate auth field {field:?}"),
            Self::AbsolutePrefixConflict {
                first_ecosystem,
                first_prefix,
                second_ecosystem,
                second_prefix,
            } => write!(
                formatter,
                "ecosystems {first_ecosystem} and {second_ecosystem} declare conflicting absolute prefixes \
                 {first_prefix:?} and {second_prefix:?}"
            ),
            Self::DriverEcosystem { registration, driver } => write!(
                formatter,
                "ecosystem {registration} registration returned a {driver} protocol driver"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

#[derive(Clone, Copy)]
pub struct ActivatedPlugin<'a> {
    driver: &'a Arc<dyn EcosystemDriver>,
}

impl<'a> ActivatedPlugin<'a> {
    #[must_use]
    pub const fn driver(self) -> &'a Arc<dyn EcosystemDriver> {
        self.driver
    }
}

pub struct PluginRegistry {
    registrations: Vec<PluginRegistration>,
    metadata_migrations: Vec<Arc<dyn peryx_storage::meta::MetadataMigration>>,
    operator_jobs: Vec<&'static dyn OperatorJob>,
    protocols: HashMap<Ecosystem, ProtocolDriver>,
    drivers: DriverSet,
    browse_paths: Vec<&'static str>,
    auth_fields: HashSet<&'static str>,
}

impl PluginRegistry {
    /// # Errors
    /// Returns the conflicting registration or reports an empty registration set.
    pub fn new(registrations: Vec<PluginRegistration>) -> Result<Self, RegistryError> {
        if registrations.is_empty() {
            return Err(RegistryError::Empty);
        }
        validate_registrations(&registrations)?;
        validate_operator_jobs(
            &registrations
                .iter()
                .flat_map(|registration| registration.operator_jobs.iter().copied())
                .collect::<Vec<_>>(),
        )?;
        Ok(Self::from_registrations(registrations))
    }

    /// # Errors
    /// Returns an error when a selected ecosystem has no compiled registration.
    pub fn activate(&self, ecosystems: impl IntoIterator<Item = Ecosystem>) -> Result<Self, RegistryError> {
        let mut active = HashSet::new();
        for ecosystem in ecosystems {
            if !self.is_installed(&ecosystem) {
                return Err(RegistryError::MissingEcosystem(ecosystem));
            }
            active.insert(ecosystem);
        }
        Self::from_active_registrations(
            self.registrations
                .iter()
                .filter(|registration| active.contains(&registration.registration.ecosystem()))
                .cloned()
                .collect(),
        )
    }

    fn from_registrations(mut registrations: Vec<PluginRegistration>) -> Self {
        registrations.sort_unstable_by_key(|registration| registration.priority);
        let operator_jobs = registrations
            .iter()
            .flat_map(|registration| registration.operator_jobs.iter().copied())
            .collect::<Vec<_>>();
        let metadata_migrations = registrations
            .iter()
            .filter_map(|registration| registration.metadata_migration.clone())
            .collect();
        let mut seen = HashSet::new();
        let browse_paths = registrations
            .iter()
            .filter_map(|registration| registration.browse)
            .flat_map(EcosystemBrowse::paths)
            .copied()
            .filter(|path| seen.insert(*path))
            .collect();
        let auth_fields = registrations
            .iter()
            .filter_map(|registration| registration.auth)
            .flat_map(EcosystemAuth::fields)
            .copied()
            .collect();
        Self {
            registrations,
            metadata_migrations,
            operator_jobs,
            protocols: HashMap::new(),
            drivers: DriverSet::default(),
            browse_paths,
            auth_fields,
        }
    }

    fn from_active_registrations(registrations: Vec<PluginRegistration>) -> Result<Self, RegistryError> {
        let mut registry = Self::from_registrations(registrations);
        let mut protocols = HashMap::new();
        let mut drivers = DriverSet::default();
        for registration in &registry.registrations {
            let ecosystem = registration.registration.ecosystem();
            let driver = registration.registration.driver();
            if driver.ecosystem() != ecosystem {
                return Err(RegistryError::DriverEcosystem {
                    registration: ecosystem,
                    driver: driver.ecosystem(),
                });
            }
            drivers = drivers.with(driver.driver_arc());
            registration.registration.register_capabilities(&mut drivers);
            protocols.insert(ecosystem, driver);
        }
        registry.protocols = protocols;
        registry.drivers = drivers;
        Ok(registry)
    }

    #[must_use]
    pub fn default_ecosystem(&self) -> Ecosystem {
        self.registrations[0].registration.ecosystem()
    }

    /// # Errors
    /// Returns the first store or owner migration error.
    pub fn migrate_metadata(
        &self,
        store: &peryx_storage::meta::MetaStore,
    ) -> Result<Vec<peryx_storage::meta::MetadataMigrationReport>, peryx_storage::meta::MetadataMigrationError> {
        self.metadata_migrations
            .iter()
            .map(|migration| store.migrate_metadata(migration.as_ref()))
            .collect()
    }

    /// # Errors
    /// Returns the first store or owner migration error.
    pub fn dry_run_metadata_migrations(
        &self,
        store: &peryx_storage::meta::MetaStore,
    ) -> Result<Vec<peryx_storage::meta::MetadataMigrationReport>, peryx_storage::meta::MetadataMigrationError> {
        self.metadata_migrations
            .iter()
            .map(|migration| store.dry_run_metadata_migration(migration.as_ref()))
            .collect()
    }

    #[must_use]
    pub fn has_metadata_migrations(&self) -> bool {
        !self.metadata_migrations.is_empty()
    }

    #[must_use]
    pub fn is_installed(&self, ecosystem: &Ecosystem) -> bool {
        self.registrations
            .iter()
            .any(|registration| registration.registration.ecosystem() == *ecosystem)
    }

    pub fn default_indexes(&self) -> impl Iterator<Item = &'static peryx_core::DefaultIndex> + '_ {
        self.registrations
            .iter()
            .flat_map(|registration| registration.registration.default_indexes())
    }

    #[must_use]
    pub fn browse_paths(&self) -> &[&'static str] {
        &self.browse_paths
    }

    pub fn absolute_prefixes(&self) -> impl Iterator<Item = (Ecosystem, &'static str)> + '_ {
        self.registrations.iter().flat_map(|registration| {
            let ecosystem = registration.registration.ecosystem();
            registration
                .registration
                .absolute_prefixes()
                .iter()
                .map(move |&prefix| (ecosystem.clone(), prefix))
        })
    }

    /// # Errors
    /// Returns an error when the ecosystem is absent or has no browse capability.
    pub async fn dispatch_browse(
        &self,
        ecosystem: Ecosystem,
        state: Arc<AppState>,
        request: Request,
    ) -> Result<Response, String> {
        let registration = self.registration(&ecosystem)?;
        let browse = registration
            .browse
            .ok_or_else(|| format!("ecosystem {ecosystem} does not provide browsing"))?;
        Ok(browse.dispatch(state, request).await)
    }

    #[must_use]
    pub const fn drivers(&self) -> &DriverSet {
        &self.drivers
    }

    pub fn register_activated_capabilities(&self, context: &mut CapabilityInstallContext<'_>) {
        context.replace_drivers(self.drivers.clone());
        for registration in &self.registrations {
            let ecosystem = registration.registration.ecosystem();
            context.register_protocol(self.protocols[&ecosystem].clone());
            if let Some(principal) = registration.rate_limit_principal {
                context.register_rate_limit_principal(ecosystem.clone(), principal);
            }
            if let Some(discovery) = registration.client_discovery {
                context.register_client_discovery(ecosystem, discovery);
            }
        }
    }

    /// # Errors
    /// Returns an error when the registration is missing or has not been activated.
    pub fn activated_plugin(&self, ecosystem: Ecosystem) -> Result<ActivatedPlugin<'_>, RegistryError> {
        if !self.is_installed(&ecosystem) {
            return Err(RegistryError::MissingEcosystem(ecosystem));
        }
        self.drivers
            .get(&ecosystem)
            .map(|driver| ActivatedPlugin { driver })
            .ok_or(RegistryError::InactiveEcosystem(ecosystem))
    }

    #[must_use]
    pub fn protocol(&self, ecosystem: &Ecosystem) -> Option<&ProtocolDriver> {
        self.protocols.get(ecosystem)
    }

    /// # Errors
    /// Returns an error when the ecosystem is absent or has no client discovery capability.
    pub fn discover_index(
        &self,
        ecosystem: &Ecosystem,
        index: peryx_driver::state::IndexDescription,
        base: Option<&BaseUrl>,
    ) -> Result<serde_json::Value, String> {
        Ok(self.client_discovery(ecosystem)?.discover_index(index, base))
    }

    /// # Errors
    /// Returns an error when the ecosystem is absent or has no client discovery capability.
    pub fn client_endpoint(&self, ecosystem: &Ecosystem, route: &str) -> Result<String, String> {
        Ok(self.client_discovery(ecosystem)?.client_endpoint(route))
    }

    /// # Errors
    /// Returns an error when no driver claims the kind, multiple drivers claim it, or compilation fails.
    pub fn compile_job(&self, config: JobConfig<'_>) -> Result<peryx_driver::jobs::PluginScheduledJob, String> {
        let mut compiled = self.registrations.iter().filter_map(|registration| {
            let ecosystem = registration.registration.ecosystem();
            self.drivers
                .get_job(&ecosystem)?
                .compile_job(config)
                .map(|result| (ecosystem, result))
        });
        let (ecosystem, result) = compiled
            .next()
            .ok_or_else(|| format!("unknown job kind {:?}", config.kind))?;
        if compiled.next().is_some() {
            return Err(format!("job kind {:?} is claimed by multiple ecosystems", config.kind));
        }
        let job = result?;
        if job.ecosystem() != ecosystem {
            return Err(format!(
                "ecosystem {ecosystem} driver returned a scheduled job for {}",
                job.ecosystem()
            ));
        }
        Ok(job)
    }

    /// # Errors
    /// Returns an error when no operator job owns `command`.
    pub fn operator_job_defaults(&self, command: &str) -> Result<OperatorJobDefaults, String> {
        Ok(self.operator_job(command)?.defaults())
    }

    /// Returns active commands and defaults in plugin priority order.
    pub fn operator_job_commands(&self) -> impl Iterator<Item = (&'static str, OperatorJobDefaults)> + '_ {
        self.operator_jobs.iter().map(|job| (job.command(), job.defaults()))
    }

    /// # Errors
    /// Returns an error when no operator job owns `command` or compilation fails.
    pub fn compile_operator_job(
        &self,
        command: &str,
        request: OperatorJobRequest<'_>,
    ) -> Result<peryx_driver::jobs::PluginScheduledJob, String> {
        let job = self.operator_job(command)?;
        let defaults = job.defaults();
        job.compile(OperatorJobOptions {
            target: request.target,
            source: request.source,
            item_limit: request.item_limit.unwrap_or(defaults.item_limit),
            concurrency: request.concurrency.unwrap_or(defaults.concurrency),
            timeout_secs: request.timeout_secs.unwrap_or(defaults.timeout_secs),
        })
    }

    #[must_use]
    pub fn default_auth_extensions(&self) -> toml::Table {
        self.registrations
            .iter()
            .filter_map(|registration| registration.auth)
            .flat_map(EcosystemAuth::defaults)
            .collect()
    }

    /// # Errors
    /// Returns an error for an unclaimed field or ecosystem validation failure.
    pub fn validate_auth_extensions(
        &self,
        values: &toml::Table,
        signing_key_configured: bool,
        token_ttl_secs: i64,
        indexes: &[PluginIndexConfig<'_>],
    ) -> Result<(), String> {
        if let Some(field) = values.keys().find(|field| !self.auth_fields.contains(field.as_str())) {
            return Err(format!("auth: unknown field `{field}`"));
        }
        let values = self.auth_extensions(values);
        for auth in self.registrations.iter().filter_map(|registration| registration.auth) {
            auth.validate(PluginAuthConfig {
                values: &owned_fields(&values, auth.fields()),
                signing_key_configured,
                token_ttl_secs,
                indexes,
            })?;
        }
        Ok(())
    }

    /// # Errors
    /// Returns an error when an ecosystem cannot install authentication services.
    pub fn install_auth_extensions(
        &self,
        context: &mut peryx_driver::serving::AuthInstallContext<'_>,
        values: &toml::Table,
    ) -> Result<(), String> {
        let values = self.auth_extensions(values);
        for auth in self.registrations.iter().filter_map(|registration| registration.auth) {
            auth.install(context, &owned_fields(&values, auth.fields()))?;
        }
        Ok(())
    }

    fn auth_extensions(&self, configured: &toml::Table) -> toml::Table {
        let mut values = self.default_auth_extensions();
        values.extend(configured.clone());
        values
    }

    /// # Errors
    /// Returns an error when an ecosystem cannot install local runtime services.
    pub fn install_drivers<S: std::hash::BuildHasher>(
        &self,
        context: &mut RuntimeInstallContext<'_>,
        settings: &HashMap<String, CompiledEcosystemSettings, S>,
    ) -> Result<(), String> {
        self.install_browse_routes(context);
        for registration in &self.registrations {
            registration
                .runtime
                .install(context, &ecosystem_settings(registration, settings))?;
        }
        Ok(())
    }

    /// # Errors
    /// Returns an error when an ecosystem cannot install distributed runtime services.
    pub fn install_distributed_drivers<S: std::hash::BuildHasher>(
        &self,
        context: &mut DistributedInstallContext<'_>,
        settings: &HashMap<String, CompiledEcosystemSettings, S>,
    ) -> Result<(), String> {
        self.install_browse_routes(context.runtime());
        for registration in &self.registrations {
            if let Some(runtime) = registration.distributed_runtime {
                runtime.install(context, &ecosystem_settings(registration, settings))?;
            } else {
                registration
                    .runtime
                    .install(context.runtime(), &ecosystem_settings(registration, settings))?;
            }
        }
        Ok(())
    }

    /// # Errors
    /// Returns the ecosystem's settings error or reports an uninstalled ecosystem.
    pub fn compile_index_settings(
        &self,
        ecosystem: &Ecosystem,
        name: &str,
        settings: &toml::Table,
    ) -> Result<Option<CompiledEcosystemSettings>, String> {
        self.registration(ecosystem)?
            .config
            .compile_index_settings(name, settings)
    }

    #[must_use]
    pub fn openapi_paths(&self, paths: PathsBuilder) -> PathsBuilder {
        self.registrations
            .iter()
            .fold(paths, |paths, registration| registration.openapi.paths(paths))
    }

    /// # Errors
    /// Returns an error when the ecosystem is absent, has no snippet capability, or rejects the format.
    pub fn snippet_text(
        &self,
        ecosystem: &Ecosystem,
        base: &BaseUrl,
        route: &str,
        uploads: bool,
        format: &str,
    ) -> Result<Option<String>, String> {
        let snippets = self
            .registration(ecosystem)?
            .snippets
            .ok_or_else(|| format!("ecosystem {ecosystem} does not provide client snippets"))?;
        snippets.text(base, route, uploads, format)
    }

    fn registration(&self, ecosystem: &Ecosystem) -> Result<&PluginRegistration, String> {
        self.registrations
            .iter()
            .find(|registration| registration.registration.ecosystem() == *ecosystem)
            .ok_or_else(|| format!("ecosystem {ecosystem} is not installed"))
    }

    fn client_discovery(&self, ecosystem: &Ecosystem) -> Result<&'static dyn ClientDiscovery, String> {
        self.registration(ecosystem)?
            .client_discovery
            .ok_or_else(|| format!("ecosystem {ecosystem} does not provide client discovery"))
    }

    fn install_browse_routes(&self, context: &mut RuntimeInstallContext<'_>) {
        if !self.browse_paths.is_empty() {
            context.register_routes(Arc::new(BrowseRoutes {
                browsers: self
                    .registrations
                    .iter()
                    .filter_map(|registration| {
                        registration
                            .browse
                            .map(|browse| (registration.registration.ecosystem(), browse))
                    })
                    .collect(),
                paths: self.browse_paths.clone(),
            }));
        }
    }

    fn operator_job(&self, command: &str) -> Result<&'static dyn OperatorJob, String> {
        self.operator_jobs
            .iter()
            .copied()
            .find(|job| job.command() == command)
            .ok_or_else(|| format!("unknown operator job command {command:?}"))
    }
}

struct BrowseRoutes {
    browsers: HashMap<Ecosystem, &'static dyn EcosystemBrowse>,
    paths: Vec<&'static str>,
}

#[derive(serde::Deserialize)]
struct BrowseQuery {
    index: String,
}

impl HttpRoutes for BrowseRoutes {
    fn routes(&self) -> Router<Arc<AppState>> {
        let browsers = Arc::new(self.browsers.clone());
        self.paths.iter().fold(Router::new(), |router, &path| {
            let browsers = browsers.clone();
            router.route(
                path,
                get(
                    move |State(state): State<Arc<AppState>>, Query(query): Query<BrowseQuery>, request: Request| {
                        let browsers = browsers.clone();
                        async move {
                            let Some(index) = state.serving.indexes.iter().find(|index| index.route == query.index)
                            else {
                                return peryx_driver::not_found();
                            };
                            let Some(browser) = browsers.get(&index.ecosystem) else {
                                return peryx_driver::not_found();
                            };
                            browser.dispatch(state, request).await
                        }
                    },
                ),
            )
        })
    }
}

fn validate_registrations(registrations: &[PluginRegistration]) -> Result<(), RegistryError> {
    let mut ecosystems = HashSet::new();
    let mut priorities = HashSet::new();
    let mut auth_fields = HashSet::new();
    let mut absolute_prefixes: Vec<(Ecosystem, &'static str)> = Vec::new();
    for registration in registrations {
        let ecosystem = registration.registration.ecosystem();
        if !ecosystems.insert(ecosystem.clone()) {
            return Err(RegistryError::DuplicateEcosystem(ecosystem));
        }
        if !priorities.insert(registration.priority) {
            return Err(RegistryError::DuplicatePriority(registration.priority));
        }
        if let Some(field) = registration
            .auth
            .into_iter()
            .flat_map(EcosystemAuth::fields)
            .find(|field| !auth_fields.insert(**field))
        {
            return Err(RegistryError::DuplicateAuthField(field));
        }
        for &prefix in registration.registration.absolute_prefixes() {
            if let Some((first_ecosystem, first_prefix)) = absolute_prefixes
                .iter()
                .find(|(_, registered)| path_prefix(prefix, registered) || path_prefix(registered, prefix))
            {
                return Err(RegistryError::AbsolutePrefixConflict {
                    first_ecosystem: first_ecosystem.clone(),
                    first_prefix,
                    second_ecosystem: ecosystem,
                    second_prefix: prefix,
                });
            }
            absolute_prefixes.push((ecosystem.clone(), prefix));
        }
    }
    Ok(())
}

fn path_prefix(prefix: &str, path: &str) -> bool {
    let mut path = path.split('/').filter(|segment| !segment.is_empty());
    prefix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .all(|segment| path.next() == Some(segment))
}

fn validate_operator_jobs(jobs: &[&dyn OperatorJob]) -> Result<(), RegistryError> {
    let mut commands = HashSet::new();
    if let Some(job) = jobs.iter().find(|job| !commands.insert(job.command())) {
        return Err(RegistryError::DuplicateOperatorJob(job.command()));
    }
    Ok(())
}

fn owned_fields(values: &toml::Table, fields: &[&str]) -> toml::Table {
    values
        .iter()
        .filter(|(key, _)| fields.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn ecosystem_settings<'a, S: std::hash::BuildHasher>(
    registration: &PluginRegistration,
    settings: &'a HashMap<String, CompiledEcosystemSettings, S>,
) -> Vec<(&'a str, &'a CompiledEcosystemSettings)> {
    let mut settings = settings
        .iter()
        .filter(|(_, settings)| settings.ecosystem() == registration.registration.ecosystem())
        .map(|(name, settings)| (name.as_str(), settings))
        .collect::<Vec<_>>();
    settings.sort_unstable_by_key(|(name, _)| *name);
    settings
}
