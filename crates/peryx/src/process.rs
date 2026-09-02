use std::path::Path;
use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::Context as _;
use axum::serve::ListenerExt as _;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, Registry};

use crate::cli::{Cli, ConfigSnippetArgs};
use crate::config::{self, Config, LogConfig, LogFormat, LogSink};
use crate::{app, logging, operator};

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;
const PUBLIC_LISTENER_FD_ENV: &str = "PERYX_INHERITED_PUBLIC_LISTENER_FD";
const AVAILABILITY_LISTENER_FD_ENV: &str = "PERYX_INHERITED_AVAILABILITY_LISTENER_FD";

#[cfg(unix)]
struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(windows)]
struct ShutdownSignals {
    ctrl_c: tokio::signal::windows::CtrlC,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    async fn cancel(mut self, cancellation: tokio_util::sync::CancellationToken) {
        let signal = tokio::select! {
            _ = self.interrupt.recv() => nix::sys::signal::Signal::SIGINT,
            _ = self.terminate.recv() => nix::sys::signal::Signal::SIGTERM,
        };
        restore_default_shutdown_signals(signal);
        cancellation.cancel();
        tracing::info!(signal = signal.as_str(), "shutdown signal received");
    }
}

#[cfg(windows)]
impl ShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            ctrl_c: tokio::signal::windows::ctrl_c()?,
        })
    }

    async fn cancel(mut self, cancellation: tokio_util::sync::CancellationToken) {
        self.ctrl_c.recv().await;
        cancellation.cancel();
        tracing::info!(signal = "Ctrl-C", "shutdown signal received");
    }
}

#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "POSIX provides no safe API for restoring default signal handlers"
)]
fn restore_default_shutdown_signals(received: nix::sys::signal::Signal) {
    let other = if received == nix::sys::signal::Signal::SIGINT {
        nix::sys::signal::Signal::SIGTERM
    } else {
        nix::sys::signal::Signal::SIGINT
    };
    for signal in [received, other] {
        // Tokio retains its handlers after listeners are dropped, so restore the process defaults explicitly.
        unsafe { nix::sys::signal::signal(signal, nix::sys::signal::SigHandler::SigDfl) }
            .expect("shutdown signal constants are valid");
    }
}

enum ShutdownControl {
    Injected,
    ProcessSignals,
}

struct ResolvedConfig {
    config: Config,
    plugins: peryx_plugin_registry::PluginRegistry,
}

fn resolve_config(
    args: &crate::cli::RuntimeArgs,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<ResolvedConfig> {
    let mut cfg = resolve_config_file(args.config.as_deref(), plugins)?;
    cfg = cfg.apply_with_plugins(config::from_env()?, plugins)?;
    cfg = cfg.apply_with_plugins(args.overlay(), plugins)?;
    let plugins = crate::server::activate_plugins(&cfg, plugins)?;
    cfg.validate_with_plugins(&plugins)?;
    Ok(ResolvedConfig { config: cfg, plugins })
}

fn resolve_config_file(path: Option<&Path>, plugins: &peryx_plugin_registry::PluginRegistry) -> anyhow::Result<Config> {
    let mut cfg = Config::with_plugins(plugins);
    if let Some(path) = path {
        cfg = cfg.apply_with_plugins(config::from_file(path.to_path_buf())?, plugins)?;
    }
    Ok(cfg)
}

fn fmt_layer<W>(format: LogFormat, writer: W) -> BoxedLayer
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Pretty => tracing_subscriber::fmt::layer().with_writer(writer).boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer().json().with_writer(writer).boxed(),
    }
}

fn install_logging(log: &LogConfig) -> anyhow::Result<Option<WorkerGuard>> {
    let (layer, guard) = logging_layer(log)?;
    tracing_subscriber::registry().with(layer).init();
    Ok(guard)
}

fn logging_layer(log: &LogConfig) -> anyhow::Result<(BoxedLayer, Option<WorkerGuard>)> {
    let filter = logging::env_filter(&log.level).context("invalid log level")?;
    let mut guard = None;
    let layer: BoxedLayer = match log.sink {
        LogSink::Stdout => fmt_layer(log.format, std::io::stdout),
        LogSink::File => {
            let path = log.file.as_ref().context("file sink without a path")?;
            let current_directory = Path::new(".");
            let dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(current_directory);
            let name = path.file_name().context("log file path has no file name")?;
            let (writer, worker) = tracing_appender::non_blocking(tracing_appender::rolling::daily(dir, name));
            guard = Some(worker);
            fmt_layer(log.format, writer)
        }
        LogSink::Journald => journald_layer(log.format)?,
        LogSink::Syslog => syslog_layer(log.format)?,
    };
    Ok((layer.with_filter(filter).boxed(), guard))
}

#[cfg(target_os = "linux")]
fn journald_layer(_format: LogFormat) -> anyhow::Result<BoxedLayer> {
    Ok(tracing_journald::layer()
        .context("connect to the systemd journal")?
        .boxed())
}

#[cfg(not(target_os = "linux"))]
fn journald_layer(_format: LogFormat) -> anyhow::Result<BoxedLayer> {
    anyhow::bail!("the journald log sink is only available on Linux")
}

#[cfg(unix)]
fn syslog_layer(format: LogFormat) -> anyhow::Result<BoxedLayer> {
    let identity = std::ffi::CString::new("peryx").expect("static identity has no NUL");
    let (options, facility) = Default::default();
    let syslog = syslog_tracing::Syslog::new(identity, options, facility).context("open syslog")?;
    Ok(fmt_layer(format, syslog))
}

#[cfg(not(unix))]
fn syslog_layer(_format: LogFormat) -> anyhow::Result<BoxedLayer> {
    anyhow::bail!("the syslog log sink requires a Unix platform")
}

pub(crate) async fn prepare_distributed_availability(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    state: &Arc<peryx_driver::AppState>,
    listener: Option<Box<dyn peryx_ha_distributed::PreparedAvailabilityListener>>,
) -> anyhow::Result<peryx_ha::PreparedAvailability<axum::Router, peryx_ha_distributed::DistributedHandle>> {
    let prepared = crate::replication::ReplicationRuntime::new(config, state)?
        .prepare(
            state,
            peryx_ha_distributed::reference_inventory(
                plugins.drivers().clone(),
                state.serving.meta.clone(),
                config.indexes.iter().map(|index| index.name.clone()).collect(),
            ),
            listener,
        )
        .await?;
    for metrics in &prepared.metrics {
        state.register_prometheus(metrics.clone());
    }
    Ok(prepared)
}

pub(crate) async fn prepare_process_availability(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    state: &Arc<peryx_driver::AppState>,
) -> anyhow::Result<Option<peryx_ha::PreparedAvailability<axum::Router, peryx_ha_distributed::DistributedHandle>>> {
    match config.availability {
        config::AvailabilityConfig::None => Ok(None),
        config::AvailabilityConfig::Dc(_) | config::AvailabilityConfig::Ha(_) => {
            prepare_distributed_availability(config, plugins, state, prepare_availability_listener(config).await?)
                .await
                .map(Some)
        }
    }
}

pub(crate) fn activate_prepared_availability(
    prepared: peryx_ha::PreparedAvailability<axum::Router, peryx_ha_distributed::DistributedHandle>,
) -> anyhow::Result<
    peryx_ha::ActiveAvailability<<peryx_ha_distributed::DistributedHandle as peryx_ha::AvailabilityHandle>::Active>,
> {
    prepared.activate()
}

struct ProcessTasks {
    cancellation: tokio_util::sync::CancellationToken,
    webhooks: Option<peryx_events::webhook::WebhookHandle>,
    scheduler: Option<tokio::task::JoinHandle<()>>,
    cache_warming: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl ProcessTasks {
    const fn new(cancellation: tokio_util::sync::CancellationToken) -> Self {
        Self {
            cancellation,
            webhooks: None,
            scheduler: None,
            cache_warming: Vec::new(),
        }
    }

    fn spawn_cache_warming(&mut self, warm: impl Future<Output = anyhow::Result<()>> + Send + 'static) {
        let cancellation = self.cancellation.child_token();
        self.cache_warming.push(tokio::spawn(async move {
            tokio::select! {
                biased;
                result = warm => result,
                () = cancellation.cancelled_owned() => Ok(()),
            }
        }));
    }

    async fn shutdown(self) -> anyhow::Result<()> {
        let mut results = Vec::with_capacity(
            self.cache_warming.len() + usize::from(self.scheduler.is_some()) + usize::from(self.webhooks.is_some()),
        );
        if let Some(webhooks) = self.webhooks {
            let result = webhooks.shutdown().await.map_err(anyhow::Error::from);
            log_shutdown_result("webhook delivery", &result);
            results.push(("webhook delivery", result));
        }
        self.cancellation.cancel();
        if let Some(scheduler) = self.scheduler {
            let result = scheduler.await.context("join local scheduler");
            log_shutdown_result("local scheduler", &result);
            results.push(("local scheduler", result));
        }
        for warming in self.cache_warming {
            let result = warming
                .await
                .context("join cache warming task")
                .and_then(std::convert::identity);
            log_shutdown_result("cache warming", &result);
            results.push(("cache warming", result));
        }
        combined_results(results)
    }
}

/// Runs the server until `shutdown` is cancelled without taking ownership of process signals.
///
/// # Errors
/// Returns an error if server setup, serving, or shutdown fails.
pub fn run_server_until_with_active_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    run_server_with_active_plugins(config, plugins, shutdown, ShutdownControl::Injected)
}

fn run_server_with_signals(config: &Config, plugins: &peryx_plugin_registry::PluginRegistry) -> anyhow::Result<()> {
    run_server_with_active_plugins(
        config,
        plugins,
        tokio_util::sync::CancellationToken::new(),
        ShutdownControl::ProcessSignals,
    )
}

fn run_server_with_active_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    shutdown: tokio_util::sync::CancellationToken,
    shutdown_control: ShutdownControl,
) -> anyhow::Result<()> {
    let listen_address = config.listen_address()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let signal_task = match shutdown_control {
            ShutdownControl::Injected => None,
            ShutdownControl::ProcessSignals => {
                let signals = ShutdownSignals::new()?;
                Some(tokio::spawn(signals.cancel(shutdown.clone())))
            }
        };
        let state = crate::server::build_state_with_active_plugins(config, plugins)?;
        crate::server::recover_job_attempts(&state)?;
        crate::server::recover_blob_uploads(&state).await?;
        let availability = prepare_process_availability(config, plugins, &state).await?;
        let result = run_prepared_process(config, listen_address, state, availability, shutdown).await;
        if let Some(signal_task) = signal_task {
            signal_task.abort();
        }
        result
    })
}

async fn run_prepared_process(
    config: &Config,
    listen_address: std::net::SocketAddr,
    state: Arc<peryx_driver::AppState>,
    mut prepared_availability: Option<
        peryx_ha::PreparedAvailability<axum::Router, peryx_ha_distributed::DistributedHandle>,
    >,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let router = crate::server::router_for(
        Arc::clone(&state),
        prepared_availability
            .as_ref()
            .map_or_else(axum::Router::new, |prepared| prepared.public_routes.clone()),
    );
    let is_replica = prepared_availability
        .as_ref()
        .is_some_and(|prepared| prepared.is_replica);
    let mut tasks = ProcessTasks::new(shutdown.clone());
    let public_server = match prepare_public_server(config, listen_address, router, shutdown.clone()).await {
        Ok(server) => server,
        Err(error) => {
            return finish_process(
                Err(error),
                tasks,
                Box::new(move || {
                    Box::pin(async move {
                        match prepared_availability {
                            Some(prepared) => {
                                let result = prepared.shutdown().await.map_err(anyhow::Error::from);
                                log_shutdown_result("availability", &result);
                                result
                            }
                            None => Ok(()),
                        }
                    })
                }),
            )
            .await;
        }
    };
    let mut availability = match prepared_availability.take() {
        Some(prepared) => match activate_prepared_availability(prepared) {
            Ok(active) => Some(active),
            Err(error) => return finish_process(Err(error), tasks, Box::new(|| Box::pin(async { Ok(()) }))).await,
        },
        None => None,
    };
    start_process_tasks(config, &state, is_replica, &mut tasks);
    let result = match (&mut availability, &mut tasks.webhooks) {
        (Some(availability), Some(webhooks)) => tokio::select! {
            result = public_server.serve() => result,
            failure = peryx_ha::ActiveAvailabilityHandle::wait_for_failure(&mut availability.handle) => Err(failure.into()),
            failure = webhooks.wait_for_failure() => Err(failure.into()),
        },
        (Some(availability), None) => tokio::select! {
            result = public_server.serve() => result,
            failure = peryx_ha::ActiveAvailabilityHandle::wait_for_failure(&mut availability.handle) => Err(failure.into()),
        },
        (None, Some(webhooks)) => tokio::select! {
            result = public_server.serve() => result,
            failure = webhooks.wait_for_failure() => Err(failure.into()),
        },
        (None, None) => public_server.serve().await,
    };
    finish_process(
        result,
        tasks,
        Box::new(move || {
            Box::pin(async move {
                match availability {
                    Some(mut active) => {
                        let result = peryx_ha::ActiveAvailabilityHandle::shutdown(&mut active.handle)
                            .await
                            .map_err(anyhow::Error::from);
                        log_shutdown_result("availability", &result);
                        result
                    }
                    None => Ok(()),
                }
            })
        }),
    )
    .await
}

fn start_process_tasks(
    config: &Config,
    state: &Arc<peryx_driver::AppState>,
    is_replica: bool,
    tasks: &mut ProcessTasks,
) {
    if !state.serving.read_only {
        tasks.webhooks = peryx_events::webhook::kick(state.serving.clone());
    }
    if !state.serving.read_only && config.jobs.mode == config::JobsMode::Local {
        let scheduler = std::sync::Arc::new(peryx_driver::jobs::JobScheduler::new(
            state.serving.clone(),
            peryx_driver::jobs::JobLimits::node_local(),
        ));
        state.register_prometheus(scheduler.metrics());
        tasks.scheduler = Some(tokio::spawn(peryx_driver::jobs::run_schedules(
            state.clone(),
            scheduler,
            config.jobs.schedules.clone(),
            tasks.cancellation.child_token(),
        )));
    }
    for index in (!state.serving.read_only && !is_replica)
        .then_some(&state.serving.indexes)
        .into_iter()
        .flatten()
    {
        if let peryx_driver::IndexKind::Cached { client, offline: false } = &index.kind {
            let client = client.clone();
            tasks.spawn_cache_warming(async move {
                client.warm().await;
                Ok(())
            });
        }
    }
}

type ShutdownFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

async fn finish_process(
    process: anyhow::Result<()>,
    tasks: ProcessTasks,
    shutdown_availability: Box<dyn FnOnce() -> ShutdownFuture + Send>,
) -> anyhow::Result<()> {
    let tasks = tasks.shutdown().await;
    let availability = shutdown_availability().await;
    combined_results([
        ("public server", process),
        ("process tasks", tasks),
        ("availability shutdown", availability),
    ])
}

fn log_shutdown_result(resource: &'static str, result: &anyhow::Result<()>) {
    match result {
        Ok(()) => tracing::info!(resource, "resource shutdown completed"),
        Err(error) => tracing::error!(resource, %error, "resource shutdown failed"),
    }
}

fn combined_results(results: impl IntoIterator<Item = (&'static str, anyhow::Result<()>)>) -> anyhow::Result<()> {
    let mut failures = results
        .into_iter()
        .filter_map(|(owner, result)| result.err().map(|error| (owner, error)))
        .collect::<Vec<_>>();
    match failures.len() {
        0 => Ok(()),
        1 => Err(failures.pop().expect("one failure was counted").1),
        _ => anyhow::bail!(
            "{}",
            failures
                .into_iter()
                .map(|(owner, error)| format!("{owner}: {error:#}"))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}

enum AvailabilityListenerTransport {
    Http,
    Tls(axum_server::tls_rustls::RustlsConfig),
}

struct ProcessAvailabilityListener {
    address: std::net::SocketAddr,
    listener: std::net::TcpListener,
    transport: AvailabilityListenerTransport,
}

impl peryx_ha_distributed::PreparedAvailabilityListener for ProcessAvailabilityListener {
    fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    fn serve(
        self: Box<Self>,
        router: axum::Router,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<peryx_ha_distributed::AvailabilityListenerFuture, peryx_ha_distributed::AvailabilityListenerError> {
        let address = self.address;
        let make_service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        match self.transport {
            AvailabilityListenerTransport::Http => {
                let listener = tokio::net::TcpListener::from_std(self.listener)
                    .map_err(peryx_ha_distributed::AvailabilityListenerError::setup)?;
                Ok(Box::pin(async move {
                    tracing::info!(%address, scheme = "http", "peryx availability listener");
                    axum::serve(listener, make_service)
                        .with_graceful_shutdown(shutdown.cancelled_owned())
                        .await
                        .map_err(peryx_ha_distributed::AvailabilityListenerError::serve)
                }))
            }
            AvailabilityListenerTransport::Tls(tls) => {
                let handle = axum_server::Handle::new();
                let server = axum_server::from_tcp_rustls(self.listener, tls)
                    .map_err(peryx_ha_distributed::AvailabilityListenerError::setup)?
                    .handle(handle.clone())
                    .serve(make_service);
                Ok(Box::pin(async move {
                    tracing::info!(%address, scheme = "https", "peryx availability listener");
                    tokio::pin!(server);
                    tokio::select! {
                        result = &mut server => result,
                        () = shutdown.cancelled_owned() => {
                            handle.shutdown();
                            server.await
                        }
                    }
                    .map_err(peryx_ha_distributed::AvailabilityListenerError::serve)
                }))
            }
        }
    }
}

async fn prepare_availability_listener(
    config: &Config,
) -> anyhow::Result<Option<Box<dyn peryx_ha_distributed::PreparedAvailabilityListener>>> {
    let Some(listener_config) = &config.availability_listener else {
        return Ok(None);
    };
    let inherited = inherited_tcp_listener(AVAILABILITY_LISTENER_FD_ENV, listener_config.bind)?;
    let listener = availability_listener_or_bind(inherited, listener_config.bind)?;
    let transport = match &listener_config.tls {
        None => return prepared_plain_availability_listener(listener).map(Some),
        Some(tls) => AvailabilityListenerTransport::Tls(load_tls_config(&tls.cert, &tls.key).await.context(
            format!("load TLS cert {} and key {}", tls.cert.display(), tls.key.display()),
        )?),
    };
    prepared_availability_listener(listener, transport).map(Some)
}

fn availability_listener_or_bind(
    inherited: Option<std::net::TcpListener>,
    bind: std::net::SocketAddr,
) -> anyhow::Result<std::net::TcpListener> {
    inherited.map_or_else(
        || std::net::TcpListener::bind(bind).with_context(|| format!("bind availability listener at {bind}")),
        Ok,
    )
}

fn prepared_availability_listener(
    listener: std::net::TcpListener,
    transport: AvailabilityListenerTransport,
) -> anyhow::Result<Box<dyn peryx_ha_distributed::PreparedAvailabilityListener>> {
    listener.set_nonblocking(true)?;
    Ok(Box::new(ProcessAvailabilityListener {
        address: listener.local_addr()?,
        listener,
        transport,
    }))
}

pub(crate) fn prepared_plain_availability_listener(
    listener: std::net::TcpListener,
) -> anyhow::Result<Box<dyn peryx_ha_distributed::PreparedAvailabilityListener>> {
    prepared_availability_listener(listener, AvailabilityListenerTransport::Http)
}

type PublicServerFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

struct PreparedPublicServer(PublicServerFuture);

impl PreparedPublicServer {
    async fn serve(self) -> anyhow::Result<()> {
        self.0.await
    }
}

async fn prepare_public_server(
    config: &Config,
    addr: std::net::SocketAddr,
    router: axum::Router,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<PreparedPublicServer> {
    let listener = public_tcp_listener(
        inherited_tcp_listener(PUBLIC_LISTENER_FD_ENV, addr)?,
        addr,
        match config.tls {
            None => "HTTP",
            Some(config::TlsConfig::Manual { .. }) => "HTTPS",
            Some(config::TlsConfig::Acme(_)) => "ACME",
        },
    )?;
    let indexes = config.indexes.len();
    let scheme = match &config.tls {
        None => "http",
        Some(config::TlsConfig::Manual { .. }) => "https",
        Some(config::TlsConfig::Acme(_)) => "https+acme",
    };
    let make_service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = match config.tls.clone() {
        None => prepared_http(listener, make_service, indexes, shutdown)?,
        Some(config::TlsConfig::Manual { cert, key }) => {
            let tls = load_tls_config(&cert, &key).await.context(format!(
                "load TLS cert {} and key {}",
                cert.display(),
                key.display()
            ))?;
            prepared_tls(listener, tls, make_service, indexes, shutdown)?
        }
        Some(config::TlsConfig::Acme(acme)) => prepared_acme(listener, acme, make_service, indexes, shutdown)?,
    };
    print_banner(&addr, indexes, scheme);
    Ok(PreparedPublicServer(server))
}

type MakeService = axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, std::net::SocketAddr>;

fn prepared_http(
    listener: std::net::TcpListener,
    make_service: MakeService,
    indexes: usize,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<PublicServerFuture> {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    Ok(Box::pin(serve_http_listener(listener, make_service, indexes, shutdown)))
}

async fn serve_http_listener(
    listener: tokio::net::TcpListener,
    make_service: MakeService,
    indexes: usize,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    let listener = listener.tap_io(|stream| {
        log_nodelay(stream.set_nodelay(true));
    });
    tracing::info!(%addr, indexes, scheme = "http", "peryx listening");
    axum::serve(listener, make_service)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await?;
    Ok(())
}

fn log_nodelay(result: std::io::Result<()>) {
    if let Err(error) = result {
        tracing::warn!(%error, "set TCP_NODELAY");
    }
}

fn prepared_tls(
    listener: std::net::TcpListener,
    tls: axum_server::tls_rustls::RustlsConfig,
    make_service: MakeService,
    indexes: usize,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<PublicServerFuture> {
    let handle = axum_server::Handle::new();
    let server = axum_server::from_tcp_rustls(listener, tls)?
        .handle(handle.clone())
        .serve(make_service);
    Ok(Box::pin(async move {
        tracing::info!(indexes, scheme = "https", "peryx listening");
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => result?,
            () = shutdown.cancelled_owned() => {
                handle.shutdown();
                server.await?;
            }
        }
        Ok(())
    }))
}

fn install_rustls_provider() {
    if rustls::crypto::aws_lc_rs::default_provider().install_default().is_ok() {
        tracing::debug!("installed rustls AWS-LC provider");
    } else {
        tracing::debug!("rustls provider already installed");
    }
}

async fn load_tls_config(cert: &Path, key: &Path) -> std::io::Result<axum_server::tls_rustls::RustlsConfig> {
    install_rustls_provider();
    axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await
}

fn print_banner(addr: &std::net::SocketAddr, indexes: usize, scheme: &str) {
    use std::io::IsTerminal as _;
    let mut stdout = std::io::stdout().lock();
    let terminal = stdout.is_terminal();
    write_banner_logged(
        &mut stdout,
        terminal,
        banner_style(&banner_environment()),
        addr,
        indexes,
        scheme,
    );
}

fn write_banner_logged(
    out: &mut dyn std::io::Write,
    terminal: bool,
    style: BannerStyle,
    addr: &std::net::SocketAddr,
    indexes: usize,
    scheme: &str,
) {
    if let Err(error) = write_banner(out, terminal, style, addr, indexes, scheme) {
        tracing::warn!(%error, "write startup banner");
    }
}

#[derive(Clone, Copy)]
struct BannerStyle {
    unicode: bool,
    colour: &'static str,
}

struct BannerEnvironment {
    locale: String,
    no_color: bool,
    color_term: String,
    term: String,
}

fn banner_environment() -> BannerEnvironment {
    BannerEnvironment {
        locale: ["LC_ALL", "LC_CTYPE", "LANG"]
            .map(|key| std::env::var(key).unwrap_or_default())
            .join(" ")
            .to_ascii_lowercase(),
        no_color: std::env::var_os("NO_COLOR").is_some(),
        color_term: std::env::var("COLORTERM").unwrap_or_default().to_ascii_lowercase(),
        term: std::env::var("TERM").unwrap_or_default().to_ascii_lowercase(),
    }
}

fn banner_style(environment: &BannerEnvironment) -> BannerStyle {
    let unicode = environment.locale.contains("utf-8") || environment.locale.contains("utf8");
    let colour = if environment.no_color {
        ""
    } else if environment.color_term.contains("truecolor") || environment.color_term.contains("24bit") {
        "\x1b[38;2;247;120;0m"
    } else if environment.term.contains("256color") {
        "\x1b[38;5;208m"
    } else {
        ""
    };
    BannerStyle { unicode, colour }
}

fn write_banner(
    out: &mut dyn std::io::Write,
    terminal: bool,
    style: BannerStyle,
    addr: &std::net::SocketAddr,
    indexes: usize,
    scheme: &str,
) -> std::io::Result<()> {
    if !terminal {
        return Ok(());
    }
    let BannerStyle { unicode, colour } = style;
    let reset = if colour.is_empty() { "" } else { "\x1b[0m" };

    let modern: &[&str] = &[
        "  ██████  ███████ ██████  ██   ██ ██   ██",
        "  ██   ██ ██      ██   ██  ██ ██   ██ ██",
        "  ██████  █████   ██████    ███     ███",
        "  ██      ██      ██   ██    ██    ██ ██",
        "  ██      ███████ ██   ██    ██   ██   ██",
    ];
    let ascii: &[&str] = &[
        "   _ __   ___ _ __ _   ___  __",
        "  | '_ \\ / _ \\ '__| | | \\ \\/ /",
        "  | |_) |  __/ |  | |_| |>  <",
        "  | .__/ \\___|_|   \\__, /_/\\_\\",
        "  |_|              |___/",
    ];
    let (art, dot, arrow) = if unicode {
        (modern, " · ", "→")
    } else {
        (ascii, " - ", "->")
    };
    writeln!(out)?;
    for line in art {
        writeln!(out, "{colour}{line}{reset}")?;
    }
    writeln!(out, "  the artifact vault{dot}v{}", env!("CARGO_PKG_VERSION"))?;
    writeln!(out)?;
    let plural = if indexes == 1 { "" } else { "es" };
    let listener = format!("  {colour}{arrow}{reset} {indexes} index{plural}, listening on {scheme}://{addr}");
    writeln!(out, "{listener}")?;
    writeln!(out)
}

fn prepared_acme(
    listener: std::net::TcpListener,
    acme: config::AcmeConfig,
    make_service: MakeService,
    indexes: usize,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<PublicServerFuture> {
    install_rustls_provider();
    let state = rustls_acme::AcmeConfig::new(acme.domains.clone())
        .contact([format!("mailto:{}", acme.contact)])
        .cache(rustls_acme::caches::DirCache::new(acme.cache_dir.clone()))
        .directory_lets_encrypt(!acme.staging)
        .state();
    let acceptor = state.axum_acceptor(state.default_rustls_config());
    let handle = axum_server::Handle::new();
    let server = axum_server::from_tcp(listener)?
        .acceptor(acceptor)
        .handle(handle.clone())
        .serve(make_service);
    Ok(Box::pin(async move {
        let acme_shutdown = tokio_util::sync::CancellationToken::new();
        let acme_task = tokio::spawn(drive_acme(state, acme_shutdown.clone()));
        tracing::info!(indexes, domains = ?acme.domains, scheme = "https+acme", "peryx listening");
        supervise_acme(
            Box::pin(async move { server.await.map_err(anyhow::Error::from) }),
            acme_task,
            shutdown,
            acme_shutdown,
            Box::new(move || handle.shutdown()),
        )
        .await
    }))
}

async fn supervise_acme(
    mut server: PublicServerFuture,
    mut acme_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown: tokio_util::sync::CancellationToken,
    acme_shutdown: tokio_util::sync::CancellationToken,
    stop_server: Box<dyn Fn() + Send>,
) -> anyhow::Result<()> {
    let mut completed_acme = None;
    let server_result = tokio::select! {
        biased;
        () = shutdown.cancelled_owned() => {
            stop_server();
            server.await
        }
        result = &mut acme_task => {
            completed_acme = Some(join_acme_task(result));
            stop_server();
            server.await
        }
        result = &mut server => result,
    };
    acme_shutdown.cancel();
    let acme_result = match completed_acme {
        Some(result) => result,
        None => join_acme_task(acme_task.await),
    };
    combined_results([("ACME listener", server_result), ("ACME task", acme_result)])
}

fn public_tcp_listener(
    inherited_listener: Option<std::net::TcpListener>,
    addr: std::net::SocketAddr,
    protocol: &str,
) -> anyhow::Result<std::net::TcpListener> {
    let listener = inherited_listener.map_or_else(
        || std::net::TcpListener::bind(addr).with_context(|| format!("bind {protocol} listener on {addr}")),
        Ok,
    )?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[cfg(unix)]
fn inherited_tcp_listener(
    variable: &'static str,
    expected: std::net::SocketAddr,
) -> anyhow::Result<Option<std::net::TcpListener>> {
    let Some(descriptor) = std::env::var_os(variable) else {
        return Ok(None);
    };
    let descriptor = descriptor
        .to_str()
        .context(format!("{variable} is not valid UTF-8"))?
        .parse::<std::os::fd::RawFd>()
        .context(format!("parse listener descriptor from {variable}"))?;
    inherited_listener_from_descriptor(
        duplicate_inherited_descriptor(descriptor, variable)?,
        expected,
        variable,
    )
    .map(Some)
}

#[cfg(unix)]
#[allow(unsafe_code, reason = "POSIX exposes inherited descriptors only as raw integers")]
fn duplicate_inherited_descriptor(
    descriptor: std::os::fd::RawFd,
    variable: &'static str,
) -> anyhow::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd as _;

    unsafe extern "C" {
        fn dup(descriptor: std::os::raw::c_int) -> std::os::raw::c_int;
    }

    // SAFETY: `dup` accepts any integer and reports closed descriptors with `EBADF`.
    let duplicate = unsafe { dup(descriptor) };
    if duplicate == -1 {
        return Err(std::io::Error::last_os_error()).context(format!("duplicate listener descriptor from {variable}"));
    }
    // SAFETY: a successful `dup` returns a new descriptor owned by this process.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) })
}

#[cfg(unix)]
fn inherited_listener_from_descriptor(
    descriptor: std::os::fd::OwnedFd,
    expected: std::net::SocketAddr,
    variable: &'static str,
) -> anyhow::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::from(descriptor);
    let actual = listener
        .local_addr()
        .context(format!("inspect listener descriptor from {variable}"))?;
    anyhow::ensure!(
        actual == expected,
        "listener descriptor from {variable} is bound to {actual}, expected {expected}"
    );
    Ok(listener)
}

#[cfg(not(unix))]
fn inherited_tcp_listener(
    _variable: &'static str,
    _expected: std::net::SocketAddr,
) -> anyhow::Result<Option<std::net::TcpListener>> {
    Ok(None)
}

fn join_acme_task(result: Result<anyhow::Result<()>, tokio::task::JoinError>) -> anyhow::Result<()> {
    result.context("join ACME task")?
}

async fn drive_acme<S, Event, Error>(
    mut state: S,
    cancellation: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()>
where
    S: futures_util::Stream<Item = Result<Event, Error>> + Unpin,
    Event: std::fmt::Debug,
    Error: std::fmt::Display,
{
    use futures_util::StreamExt as _;
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            result = state.next() => match result {
                Some(Ok(event)) => tracing::info!(?event, "acme event"),
                Some(Err(error)) => anyhow::bail!("ACME state failed: {error}"),
                None => anyhow::bail!("ACME state reached unexpected EOF"),
            },
        }
    }
}

fn config_snippet(args: &ConfigSnippetArgs, plugins: &peryx_plugin_registry::PluginRegistry) -> anyhow::Result<String> {
    let config = resolve_config_file(args.config.as_deref(), plugins)?;
    let plugins = crate::server::activate_plugins(&config, plugins)?;
    app::config_snippet_with_plugins(&config, &plugins, &args.index, &args.base_url, &args.format)
}

fn print_openapi(plugins: &peryx_plugin_registry::PluginRegistry) -> anyhow::Result<()> {
    let schema = crate::api::openapi_json_with_plugins(plugins);
    std::io::Write::write_all(&mut std::io::stdout(), schema.as_bytes()).context("write OpenAPI schema")
}

/// # Errors
/// Returns the command failure.
pub fn run(cli: Cli) -> anyhow::Result<()> {
    run_with_plugins(cli, &crate::compiled_plugins())
}

/// # Errors
/// Returns an error if command configuration, setup, or execution fails.
pub fn run_with_plugins(cli: Cli, plugins: &peryx_plugin_registry::PluginRegistry) -> anyhow::Result<()> {
    match cli.command {
        crate::cli::Command::Serve(args) => {
            let ResolvedConfig { config, plugins } = resolve_config(&args, plugins)?;
            logging::validate(&config.log)?;
            let _guard = install_logging(&config.log)?;
            run_server_with_signals(&config, &plugins)
        }
        crate::cli::Command::Init(args) => {
            let ResolvedConfig { config, .. } = resolve_config(&args, plugins)?;
            logging::validate(&config.log)?;
            let _guard = install_logging(&config.log)?;
            app::init(&config)
        }
        crate::cli::Command::BootstrapAdministrator(args) => {
            let ResolvedConfig { config, plugins } = resolve_config(&args.runtime, plugins)?;
            app::bootstrap_administrator_with_plugins(
                &config,
                &plugins,
                &args,
                &mut std::io::stdin(),
                &mut std::io::stdout(),
            )
        }
        crate::cli::Command::Revocation(command) => {
            app::revocation(&command, &mut std::io::stdin(), &mut std::io::stdout())
        }
        crate::cli::Command::Config(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            app::config_check_with_active_plugins(&config, &plugins, &mut std::io::stdout())
        }
        crate::cli::Command::ConfigSnippet(args) => {
            print!("{}", config_snippet(&args, plugins)?);
            Ok(())
        }
        crate::cli::Command::Index(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            app::index_with_plugins(&config, &plugins, &command, &mut std::io::stdout())
        }
        crate::cli::Command::Job(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            app::job_with_active_plugins(&config, &plugins, &command, &mut std::io::stdout())
        }
        crate::cli::Command::Cache(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            app::cache_with_plugins(&config, &plugins, &command, &mut std::io::stdout())
        }
        crate::cli::Command::Backup(command) => match command {
            crate::cli::BackupCommand::Create(args) => {
                let ResolvedConfig { config, plugins } = resolve_config(&args.runtime, plugins)?;
                operator::backup_create_with_plugins(&config, &plugins, &args.path, &mut std::io::stdout())
            }
            crate::cli::BackupCommand::Verify(args) => {
                operator::backup_verify_with_plugins(&args.path, plugins, &mut std::io::stdout())
            }
        },
        crate::cli::Command::Restore(args) => {
            operator::restore(&args.path, &args.data_dir, args.force, &mut std::io::stdout())
        }
        crate::cli::Command::ImportDir(args) => {
            let ResolvedConfig { config, plugins } = resolve_config(&args.runtime, plugins)?;
            operator::import_dir_with_plugins(&config, &plugins, &args.index, &args.dir, &mut std::io::stdout())
        }
        crate::cli::Command::Policy(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            app::policy_with_plugins(&config, &plugins, &command, &mut std::io::stdout())
        }
        crate::cli::Command::Quota(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            app::quota_with_plugins(&config, &plugins, &command, &mut std::io::stdout())
        }
        crate::cli::Command::Retention(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            app::retention_with_plugins(&config, &plugins, &command, &mut std::io::stdout())
        }
        crate::cli::Command::Writer(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            match command {
                crate::cli::WriterCommand::Promote(args) => {
                    operator::promote_writer_with_plugins(&config, &plugins, &args.replacement, &mut std::io::stdout())
                }
                crate::cli::WriterCommand::Claim(_) => {
                    operator::claim_writer_with_plugins(&config, &plugins, &mut std::io::stdout())
                }
            }
        }
        crate::cli::Command::Prefetch(command) => {
            let ResolvedConfig { config, plugins } = resolve_config(command.runtime_args(), plugins)?;
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(crate::prefetch::run_with_active_plugins(
                &config,
                &plugins,
                &command,
                &mut std::io::stdout(),
            ))
        }
        crate::cli::Command::Openapi => print_openapi(plugins),
        #[cfg(feature = "self-update")]
        crate::cli::Command::SelfManage(crate::cli::SelfCommand::Update) => self_update(),
    }
}

#[cfg(feature = "self-update")]
fn self_update() -> anyhow::Result<()> {
    let mut updater = axoupdater::AxoUpdater::new_for("peryx");
    updater.load_receipt().context(
        "no install receipt found; `self update` serves installer-based installs only \
         (reinstall with the install script, or update via the tool that installed peryx)",
    )?;
    let Some(result) = updater.run_sync()? else {
        println!("{}", update_message(None));
        return Ok(());
    };
    println!("{}", update_message(Some(&result.new_version_tag)));
    Ok(())
}

#[cfg(feature = "self-update")]
fn update_message(version: Option<&str>) -> String {
    version.map_or_else(
        || "peryx is already up to date".to_owned(),
        |version| format!("updated to {version}"),
    )
}

#[cfg(test)]
#[path = "../tests/unit/process_tests.rs"]
mod tests;
