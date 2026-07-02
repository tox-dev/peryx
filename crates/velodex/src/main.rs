//! velodex entrypoint. This shell reads the real environment and installs the global tracing
//! subscriber; the testable logic lives in the library crate. Coverage excludes this file.

use std::path::Path;

use anyhow::Context as _;
use axum::serve::ListenerExt as _;
use clap::Parser as _;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, Registry};

use velodex::cli::Cli;
use velodex::config::{self, Config, LogConfig, LogFormat, LogSink};
use velodex::{app, logging};

// Requests alternate small JSON pages with wheel-sized streams; mimalloc keeps the
// allocation-heavy transform path off the system allocator's locks.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

fn resolve_config(args: &velodex::cli::RuntimeArgs) -> anyhow::Result<Config> {
    let mut cfg = Config::default();
    if let Some(path) = &args.config {
        cfg = cfg.apply(config::from_file(path.clone())?)?;
    }
    cfg = cfg.apply(args.overlay())?;
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
    let identity = std::ffi::CString::new("velodex").expect("static identity has no NUL");
    let (options, facility) = Default::default();
    let syslog = syslog_tracing::Syslog::new(identity, options, facility).context("open syslog")?;
    Ok(fmt_layer(format, syslog))
}

#[cfg(not(unix))]
fn syslog_layer(_format: LogFormat) -> anyhow::Result<BoxedLayer> {
    anyhow::bail!("the syslog log sink requires a Unix platform")
}

fn run_server(config: &Config) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        let state = velodex::server::build_state(config)?;
        for index in &state.indexes {
            if let velodex_http::IndexKind::Mirror(client) = &index.kind {
                let client = client.clone();
                tokio::spawn(async move { client.warm().await });
            }
        }
        let refresher = state.clone();
        tokio::spawn(async move {
            // Frequent ticks keep detection latency low; each sweep only touches pages whose own
            // freshness window (upstream Cache-Control, or the configured fallback) has lapsed.
            let mut ticker = tokio::time::interval(std::time::Duration::from_mins(1));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match velodex_http::cache::refresh_stale_pages(&refresher).await {
                    Ok(summary) if summary.checked > 0 => {
                        tracing::info!(
                            checked = summary.checked,
                            changed = summary.changed,
                            "background refresh sweep"
                        );
                    }
                    Ok(_) => {}
                    Err(err) => tracing::error!(error = ?err, "background refresh sweep failed"),
                }
            }
        });
        let router = velodex::server::router_for(state);
        let addr = format!("{}:{}", config.host, config.port);
        // Nagle batches the small chunked frames the streaming transformer emits; disable it so
        // page bytes reach resolvers the moment they exist.
        let listener = tokio::net::TcpListener::bind(&addr).await?.tap_io(|stream| {
            let _ = stream.set_nodelay(true);
        });
        tracing::info!(%addr, indexes = config.indexes.len(), "velodex listening");
        axum::serve(listener, router).await?;
        anyhow::Ok(())
    })
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        velodex::cli::Command::Serve(args) => {
            let config = resolve_config(&args)?;
            logging::validate(&config.log)?;
            let _guard = install_logging(&config.log)?;
            run_server(&config)
        }
        velodex::cli::Command::Init(args) => {
            let config = resolve_config(&args)?;
            logging::validate(&config.log)?;
            let _guard = install_logging(&config.log)?;
            app::init(&config)
        }
        velodex::cli::Command::Openapi => {
            print!("{}", velodex_http::api::openapi_json());
            Ok(())
        }
        #[cfg(feature = "self-update")]
        velodex::cli::Command::SelfManage(velodex::cli::SelfCommand::Update) => self_update(),
    }
}

/// Replace this binary with the newest GitHub release, through the receipt the shell or PowerShell
/// installer wrote. Copies installed by pip or cargo have no receipt and are refused, so each
/// install method updates through its own package manager.
#[cfg(feature = "self-update")]
fn self_update() -> anyhow::Result<()> {
    let mut updater = axoupdater::AxoUpdater::new_for("velodex");
    updater.load_receipt().context(
        "no install receipt found; `self update` serves installer-based installs only \
         (reinstall with the install script, or update via the tool that installed velodex)",
    )?;
    match updater.run_sync()? {
        Some(result) => println!("updated to {}", result.new_version_tag),
        None => println!("velodex is already up to date"),
    }
    Ok(())
}
