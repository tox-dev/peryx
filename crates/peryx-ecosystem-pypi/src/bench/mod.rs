pub mod packages;
pub mod servers;
pub mod workloads;

#[derive(Debug, Clone, Default, clap::Args)]
#[group(skip)]
pub struct Options {}

/// Run the `PyPI` suite: every workload not in `skip`, against every server named in `only`.
///
/// Parts, any of which `--skip` leaves out by name: `install`, `pip` (the pip client inside the
/// install workload; uv still runs), `throughput`, `parallel`, `metadata`, `load`, `endpoints`.
///
/// # Errors
/// Returns an error when a server cannot start or a workload against a healthy server fails.
pub async fn run(
    context: &peryx_bench_core::context::BenchmarkContext,
    _options: &Options,
    rounds: usize,
    skip: &[String],
    only: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    let servers: Vec<_> = servers::all()
        .into_iter()
        .filter(|server| only.is_empty() || only.split(',').any(|name| name == server.name))
        .collect();
    let enabled = |part: &str| !skip.iter().any(|skipped| skipped.eq_ignore_ascii_case(part));
    if enabled("install") {
        let clients: &[&str] = if enabled("pip") { &["uv", "pip"] } else { &["uv"] };
        workloads::installs(context, &servers, clients, rounds, http).await?;
    }
    if enabled("throughput") {
        workloads::throughput(context, &servers, rounds, http).await?;
    }
    if enabled("parallel") {
        workloads::fleet(context, &servers, rounds, http).await?;
    }
    if enabled("metadata") {
        workloads::metadata(context, &servers, rounds, http).await?;
    }
    if enabled("load") {
        workloads::load(context, &servers, &[1, 32], rounds, http).await?;
    }
    if enabled("endpoints") {
        workloads::endpoints(context, &servers, rounds, http).await?;
    }
    Ok(())
}
