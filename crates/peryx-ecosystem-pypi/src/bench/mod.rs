mod packages;
mod servers;
mod workloads;

use anyhow::bail;
use clap::Command;
use peryx_bench_core::suite::{BenchmarkRun, BenchmarkSuite};

struct PypiBenchmarkSuite;

static SUITE: PypiBenchmarkSuite = PypiBenchmarkSuite;

pub static BENCHMARK_SUITE: &dyn BenchmarkSuite = &SUITE;

#[async_trait::async_trait]
impl BenchmarkSuite for PypiBenchmarkSuite {
    fn name(&self) -> &'static str {
        "pypi"
    }

    fn configure(&self, command: Command) -> Command {
        command.about("Benchmark PyPI package serving")
    }

    async fn run(&self, run: BenchmarkRun<'_>) -> anyhow::Result<()> {
        run_suite(run.context, run.rounds, run.skip, run.only, run.http).await
    }
}

/// Run the `PyPI` suite: every workload not in `skip`, against every server named in `only`.
///
/// Parts, any of which `--skip` leaves out by name: `install`, `pip` (the pip client inside the
/// install workload; uv still runs), `throughput`, `parallel`, `metadata`, `load`, `endpoints`.
///
/// # Errors
/// Returns an error when a server cannot start or a workload against a healthy server fails.
async fn run_suite(
    context: &peryx_bench_core::context::BenchmarkContext,
    rounds: usize,
    skip: &[String],
    only: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    let available = servers::all();
    let requested = (!only.is_empty()).then(|| only.split(',').collect::<Vec<_>>());
    let unknown = requested
        .as_deref()
        .unwrap_or_default()
        .iter()
        .copied()
        .filter(|name| !available.iter().any(|server| server.name == *name))
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        bail!(
            "unknown server selectors: {}; valid selectors: {}",
            unknown.join(", "),
            available
                .iter()
                .map(|server| server.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let servers: Vec<_> = available
        .into_iter()
        .filter(|server| requested.as_ref().is_none_or(|names| names.contains(&server.name)))
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

#[cfg(test)]
#[path = "../../tests/unit/bench/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/bench/workload_tests.rs"]
mod workload_tests;

#[cfg(test)]
#[path = "../../tests/unit/bench/test_support.rs"]
mod test_support;
