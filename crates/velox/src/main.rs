//! velox entrypoint. This shell reads the real environment and installs the global tracing
//! subscriber; the testable logic lives in the library crate. Coverage excludes this file.

use std::path::Path;

use anyhow::Context as _;
use clap::Parser as _;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, Registry};

use velox::cli::Cli;
use velox::config::{self, Config, LogConfig, LogFormat, LogSink};
use velox::{app, logging};

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

fn resolve_config(cli: &Cli) -> anyhow::Result<Config> {
    let mut cfg = Config::default();
    if let Some(path) = &cli.config {
        cfg = cfg.apply(config::from_file(path.clone())?);
    }
    cfg = cfg.apply(cli.overlay());
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
    let identity = std::ffi::CString::new("velox").expect("static identity has no NUL");
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
        let router = velox::server::build_router(config)?;
        let addr = format!("{}:{}", config.host, config.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!(%addr, upstream = %config.upstream_url, "velox listening");
        axum::serve(listener, router).await?;
        anyhow::Ok(())
    })
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = resolve_config(&cli)?;
    logging::validate(&config.log)?;
    let _guard = install_logging(&config.log)?;
    match cli.command {
        velox::cli::Command::Serve => run_server(&config),
        velox::cli::Command::Init => app::init(&config),
    }
}
