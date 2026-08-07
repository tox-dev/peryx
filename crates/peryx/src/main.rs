//! peryx entrypoint. This shell reads the real environment and installs the global tracing
//! subscriber; the testable logic lives in the library crate. Coverage excludes this file.

use std::path::Path;

use anyhow::Context as _;
use axum::serve::ListenerExt as _;
use clap::Parser as _;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, Registry};

use peryx::cli::{Cli, ConfigSnippetArgs};
use peryx::config::{self, Config, LogConfig, LogFormat, LogSink};
use peryx::{app, logging, operator};

// Requests alternate small JSON pages with large artifact streams; mimalloc keeps the
// allocation-heavy transform path off the system allocator's locks.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

fn resolve_config(args: &peryx::cli::RuntimeArgs) -> anyhow::Result<Config> {
    let mut cfg = resolve_config_file(args.config.as_deref())?;
    cfg = cfg.apply(config::from_env()?)?;
    cfg = cfg.apply(args.overlay())?;
    cfg.validate()?;
    Ok(cfg)
}

fn resolve_config_file(path: Option<&Path>) -> anyhow::Result<Config> {
    let mut cfg = Config::default();
    if let Some(path) = path {
        cfg = cfg.apply(config::from_file(path.to_path_buf())?)?;
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
    let filter = logging::env_filter(&log.level).context("invalid log level")?;
    let mut guard = None;
    let layer: BoxedLayer = match log.sink {
        LogSink::Stdout => fmt_layer(log.format, std::io::stdout),
        LogSink::File => {
            let path = log.file.as_ref().context("file sink without a path")?;
            let dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let name = path.file_name().context("log file path has no file name")?;
            let (writer, worker) = tracing_appender::non_blocking(tracing_appender::rolling::daily(dir, name));
            guard = Some(worker);
            fmt_layer(log.format, writer)
        }
        LogSink::Journald => journald_layer(log.format)?,
        LogSink::Syslog => syslog_layer(log.format)?,
    };
    tracing_subscriber::registry().with(layer.with_filter(filter)).init();
    Ok(guard)
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

/// Assemble the transfer coordinator the availability listener drives, resolving each configured
/// datacenter to the address its change-feed answers on and authenticating every frontier probe with the
/// node's replication token.
fn availability_coordinator(
    config: &Config,
) -> anyhow::Result<std::sync::Arc<peryx::availability::TransferCoordinator>> {
    let peers: Vec<(String, String)> = config
        .dc_membership
        .as_ref()
        .map(|membership| {
            membership
                .members
                .iter()
                .map(|member| (member.dc.clone(), member.address.clone()))
                .collect()
        })
        .unwrap_or_default();
    let token = if config.availability.is_distributed_mode() {
        let replication = config
            .availability
            .replication()
            .expect("replication role guarantees config");
        let token = match replication {
            config::ReplicationConfig::Primary { token, .. } | config::ReplicationConfig::Replica { token, .. } => {
                token
            }
        };
        token
            .read()
            .context("read the replication token for authority transfers")?
    } else {
        String::new()
    };
    let frontier: std::sync::Arc<dyn peryx::availability::FrontierSource> =
        std::sync::Arc::new(peryx::availability::RosterFrontierSource::new(peers, token));
    Ok(std::sync::Arc::new(peryx::availability::TransferCoordinator::new(
        frontier,
    )))
}

fn run_server(config: &Config) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        let state = peryx::server::build_state(config)?;
        let mut is_replica = false;
        let mut router = peryx::server::router_for(state.clone());
        let mut raft_peer_router: Option<axum::Router> = None;
        let _replication_handle = if config.availability.is_distributed_mode() {
            let replication = peryx::replication::ReplicationRuntime::new(config, &state)?;
            // Held for the process lifetime: dropping the handle shuts the ownership Raft runtime down. The
            // mutation path reaches the same group through the state registration.
            let consensus = replication.ignite_consensus().await?;
            if let Some(consensus) = &consensus {
                state.set_ownership_authority(consensus.authority.clone());
                state.set_control_plane(std::sync::Arc::new(peryx_driver::state::ControlPlane::new(
                    consensus.control.clone(),
                    state.clock.clone(),
                )));
                raft_peer_router = Some(consensus.peer_router.clone());
            }
            is_replica = replication.is_replica();
            register_availability_services(config, &state, replication.reclamation_frontiers())?;
            router = replication.mount(router);

            // Same-datacenter peers query this node for its placement receipts on the token-gated replication
            // surface, alongside the blob endpoint, so a write elsewhere in the DC can gather it into quorum.
            if let Some(receipts) = peryx::server::receipt_endpoint_router(config, &state.serving.blobs)? {
                router = router.merge(receipts);
            }
            // Remote datacenters query this node for how far it has durably applied an authority's metadata,
            // so an `ha` write elsewhere can prove its operation remote-durable.
            if let Some(frontiers) = peryx::server::frontier_endpoint_router(config, &state)? {
                router = router.merge(frontiers);
            }
            replication.start()?
        } else {
            None
        };
        peryx::server::recover_job_attempts(&state)?;
        if !state.read_only && config.jobs.mode == config::JobsMode::Local {
            let scheduler = std::sync::Arc::new(peryx_driver::jobs::JobScheduler::new(
                state.serving.clone(),
                peryx_driver::jobs::JobLimits::node_local(),
            ));
            state.register_prometheus(scheduler.metrics());
            // One bounded timer drives every configured schedule, fanning each fire out to the
            // registered jobs, rather than a task and timer per cached page or upload.
            tokio::spawn(peryx_driver::jobs::run_schedules(
                state.clone(),
                scheduler,
                config.jobs.schedules.clone(),
                tokio_util::sync::CancellationToken::new(),
            ));
        }
        if let (Some(listener_config), Some(posture)) = (
            config.availability_listener.clone(),
            peryx::availability::AvailabilityPosture::from_config(&config.availability),
        ) {
            let coordinator = availability_coordinator(config)?;
            let mut listener_router = peryx::availability::router(state.clone(), posture, coordinator);
            // Peers dial this node's advertised address for raft RPCs, so the receive-side router rides
            // the peer-facing availability listener, not the public package routes. Without it a voter
            // sends RPCs but answers none and the group never reaches quorum.
            if let Some(peer_router) = &raft_peer_router {
                listener_router = listener_router.merge(peer_router.clone());
            }
            tokio::spawn(serve_availability_listener(listener_config, listener_router));
        }
        if !state.read_only && !is_replica {
            for index in &state.indexes {
                if let peryx_driver::IndexKind::Cached { client, offline: false } = &index.kind {
                    let client = client.clone();
                    tokio::spawn(async move { client.warm().await });
                }
            }
        }
        let addr: std::net::SocketAddr = format!("{}:{}", config.host, config.port)
            .parse()
            .with_context(|| format!("parse listen address {}:{}", config.host, config.port))?;
        let indexes = config.indexes.len();
        let scheme = match &config.tls {
            None => "http",
            Some(config::TlsConfig::Manual { .. }) => "https",
            Some(config::TlsConfig::Acme(_)) => "https+acme",
        };
        print_banner(&addr, indexes, scheme);
        let make_service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        match config.tls.clone() {
            None => {
                // Nagle batches the small chunked frames the streaming transformer emits; disable it
                // so page bytes reach resolvers the moment they exist.
                let listener = tokio::net::TcpListener::bind(&addr)
                    .await
                    .with_context(|| format!("bind HTTP listener on {addr}"))?
                    .tap_io(|stream| {
                        let _ = stream.set_nodelay(true);
                    });
                tracing::info!(%addr, indexes, scheme = "http", "peryx listening");
                axum::serve(listener, make_service).await?;
            }
            Some(config::TlsConfig::Manual { cert, key }) => {
                let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                    .await
                    .with_context(|| format!("load TLS cert {} and key {}", cert.display(), key.display()))?;
                tracing::info!(%addr, indexes, scheme = "https", "peryx listening");
                axum_server::bind_rustls(addr, tls).serve(make_service).await?;
            }
            Some(config::TlsConfig::Acme(acme)) => {
                serve_acme(addr, acme, make_service, indexes).await?;
            }
        }
        anyhow::Ok(())
    })
}

/// Register the background availability services this node's scheduled jobs drive: the cross-datacenter
/// blob copier and the filesystem placement reconciler both need the local content store, and the blob
/// reclamation selector needs only the roster. Each returns `None` when this node does that work nowhere.
fn register_availability_services(
    config: &Config,
    state: &std::sync::Arc<peryx_driver::AppState>,
    frontiers: std::sync::Arc<dyn peryx_ha::ReclamationFrontiers>,
) -> anyhow::Result<()> {
    if let Some(store) = state.blobs.filesystem_store() {
        if let Some(copier) =
            peryx::availability::CrossDcBlobCopier::from_config(config, store.clone(), state.blobs.backend_id())?
        {
            state.set_cross_dc_copier(copier.bind(state.serving.clone()));
        }
        if let Some(reconciler) =
            peryx::availability::FilesystemPlacementReconciler::from_config(config, store.clone())?
        {
            state.set_placement_reconciler(reconciler.bind(state.serving.clone()));
        }
    }
    if let Some(reclaimer) = peryx::availability::BlobReclamationSelector::from_config(config, frontiers) {
        state.set_blob_reclaimer(reclaimer.bind(state.serving.clone()));
    }
    Ok(())
}

/// Serve the private availability control listener on its own socket, off the main serve path so a
/// `dc` or `ha` node exposes administration without touching the public package routes. Bind or TLS
/// failure logs and stops this listener alone, leaving package serving up.
async fn serve_availability_listener(config: config::AvailabilityListenerConfig, router: axum::Router) {
    let bind = config.bind;
    let make_service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
    let result = match config.tls {
        None => match tokio::net::TcpListener::bind(bind).await {
            Ok(listener) => {
                tracing::info!(%bind, scheme = "http", "peryx availability listener");
                axum::serve(listener, make_service).await
            }
            Err(error) => Err(error),
        },
        Some(tls) => match axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key).await {
            Ok(rustls) => {
                tracing::info!(%bind, scheme = "https", "peryx availability listener");
                axum_server::bind_rustls(bind, rustls).serve(make_service).await
            }
            Err(error) => Err(error),
        },
    };
    if let Err(error) = result {
        tracing::error!(%bind, %error, "availability listener stopped");
    }
}

/// Prints the startup banner once, on a TTY only, so piped and CI output stays clean. Two builds,
/// following the brand guidelines: UTF-8 locales get the Unicode block, older terminals the ASCII
/// form; colour is truecolor or 256-colour amber, and none under `NO_COLOR` or a basic `TERM`. The
/// structured `tracing` line still carries the machine-readable listen event.
fn print_banner(addr: &std::net::SocketAddr, indexes: usize, scheme: &str) {
    use std::io::IsTerminal as _;
    if !std::io::stdout().is_terminal() {
        return;
    }
    let env_has = |key: &str, needle: &str| std::env::var(key).is_ok_and(|v| v.to_ascii_lowercase().contains(needle));
    let unicode = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .any(|k| env_has(k, "utf-8") || env_has(k, "utf8"));
    let colour = if std::env::var_os("NO_COLOR").is_some() {
        ""
    } else if env_has("COLORTERM", "truecolor") || env_has("COLORTERM", "24bit") {
        "\x1b[38;2;247;120;0m"
    } else if env_has("TERM", "256color") {
        "\x1b[38;5;208m"
    } else {
        ""
    };
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
    println!();
    for line in art {
        println!("{colour}{line}{reset}");
    }
    println!("  the artifact vault{dot}v{}", env!("CARGO_PKG_VERSION"));
    println!();
    let plural = if indexes == 1 { "" } else { "es" };
    println!("  {colour}{arrow}{reset} {indexes} index{plural}, listening on {scheme}://{addr}");
    println!();
}

/// Serve HTTPS with certificates obtained and renewed automatically from Let's Encrypt. The ACME
/// event stream runs in the background so renewals and the TLS-ALPN-01 challenge are handled without
/// blocking traffic. Excluded from coverage: it drives a live ACME provider.
async fn serve_acme(
    addr: std::net::SocketAddr,
    acme: config::AcmeConfig,
    make_service: axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, std::net::SocketAddr>,
    indexes: usize,
) -> anyhow::Result<()> {
    use futures_util::StreamExt as _;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut state = rustls_acme::AcmeConfig::new(acme.domains.clone())
        .contact([format!("mailto:{}", acme.contact)])
        .cache(rustls_acme::caches::DirCache::new(acme.cache_dir.clone()))
        .directory_lets_encrypt(!acme.staging)
        .state();
    let acceptor = state.axum_acceptor(state.default_rustls_config());
    tokio::spawn(async move {
        loop {
            match state.next().await {
                Some(Ok(event)) => tracing::info!(?event, "acme event"),
                Some(Err(err)) => tracing::error!(%err, "acme error"),
                None => break,
            }
        }
    });
    tracing::info!(%addr, indexes, domains = ?acme.domains, scheme = "https+acme", "peryx listening");
    axum_server::bind(addr).acceptor(acceptor).serve(make_service).await?;
    anyhow::Ok(())
}

fn print_config_snippet(args: &ConfigSnippetArgs) -> anyhow::Result<()> {
    let config = resolve_config_file(args.config.as_deref())?;
    print!(
        "{}",
        app::config_snippet(&config, &args.index, &args.base_url, &args.format)?
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        peryx::cli::Command::Serve(args) => {
            let config = resolve_config(&args)?;
            logging::validate(&config.log)?;
            let _guard = install_logging(&config.log)?;
            run_server(&config)
        }
        peryx::cli::Command::Init(args) => {
            let config = resolve_config(&args)?;
            logging::validate(&config.log)?;
            let _guard = install_logging(&config.log)?;
            app::init(&config)
        }
        peryx::cli::Command::BootstrapAdministrator(args) => {
            let config = resolve_config(&args.runtime)?;
            app::bootstrap_administrator(&config, &args, &mut std::io::stdin(), &mut std::io::stdout())
        }
        peryx::cli::Command::Revocation(command) => {
            app::revocation(&command, &mut std::io::stdin(), &mut std::io::stdout())
        }
        peryx::cli::Command::Config(command) => {
            let config = resolve_config(command.runtime_args())?;
            app::config_check(&config, &mut std::io::stdout())
        }
        peryx::cli::Command::ConfigSnippet(args) => print_config_snippet(&args),
        peryx::cli::Command::Index(command) => {
            let config = resolve_config(command.runtime_args())?;
            app::index(&config, &command, &mut std::io::stdout())
        }
        peryx::cli::Command::Job(command) => {
            let config = resolve_config(command.runtime_args())?;
            app::job(&config, &command, &mut std::io::stdout())
        }
        peryx::cli::Command::Cache(command) => {
            let config = resolve_config(command.runtime_args())?;
            app::cache(&config, &command, &mut std::io::stdout())
        }
        peryx::cli::Command::Backup(command) => match command {
            peryx::cli::BackupCommand::Create(args) => {
                let config = resolve_config(&args.runtime)?;
                operator::backup_create(&config, &args.path, &mut std::io::stdout())
            }
            peryx::cli::BackupCommand::Verify(args) => operator::backup_verify(&args.path, &mut std::io::stdout()),
        },
        peryx::cli::Command::Restore(args) => {
            operator::restore(&args.path, &args.data_dir, args.force, &mut std::io::stdout())
        }
        peryx::cli::Command::ImportDir(args) => {
            let config = resolve_config(&args.runtime)?;
            operator::import_dir(&config, &args.index, &args.dir, &mut std::io::stdout())
        }
        peryx::cli::Command::Policy(command) => {
            let config = resolve_config(command.runtime_args())?;
            app::policy(&config, &command, &mut std::io::stdout())
        }
        peryx::cli::Command::Quota(command) => {
            let config = resolve_config(command.runtime_args())?;
            app::quota(&config, &command, &mut std::io::stdout())
        }
        peryx::cli::Command::Retention(command) => {
            let config = resolve_config(command.runtime_args())?;
            app::retention(&config, &command, &mut std::io::stdout())
        }
        peryx::cli::Command::Writer(command) => {
            let config = resolve_config(command.runtime_args())?;
            match command {
                peryx::cli::WriterCommand::Promote(args) => {
                    operator::promote_writer(&config, &args.replacement, &mut std::io::stdout())
                }
                peryx::cli::WriterCommand::Claim(_) => operator::claim_writer(&config, &mut std::io::stdout()),
            }
        }
        peryx::cli::Command::Prefetch(command) => {
            let config = resolve_config(command.runtime_args())?;
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(peryx::prefetch::run(&config, &command, &mut std::io::stdout()))
        }
        peryx::cli::Command::Openapi => {
            print!("{}", peryx::api::openapi_json());
            Ok(())
        }
        #[cfg(feature = "self-update")]
        peryx::cli::Command::SelfManage(peryx::cli::SelfCommand::Update) => self_update(),
    }
}

/// Replace this binary with the newest GitHub release, through the receipt the shell or PowerShell
/// installer wrote. Copies installed by pip or cargo have no receipt and are refused, so each
/// install method updates through its own package manager.
#[cfg(feature = "self-update")]
fn self_update() -> anyhow::Result<()> {
    let mut updater = axoupdater::AxoUpdater::new_for("peryx");
    updater.load_receipt().context(
        "no install receipt found; `self update` serves installer-based installs only \
         (reinstall with the install script, or update via the tool that installed peryx)",
    )?;
    match updater.run_sync()? {
        Some(result) => println!("updated to {}", result.new_version_tag),
        None => println!("peryx is already up to date"),
    }
    Ok(())
}
