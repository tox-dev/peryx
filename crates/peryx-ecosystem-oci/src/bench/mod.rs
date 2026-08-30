mod images;
mod servers;
mod workloads;

use clap::{Arg, ArgAction, Command};
use peryx_bench_core::suite::{BenchmarkRun, BenchmarkSuite};
use std::path::{Path, PathBuf};

const MIRROR_ARG: &str = "oci-mirror";

struct OciBenchmarkSuite;

static SUITE: OciBenchmarkSuite = OciBenchmarkSuite;

#[derive(Clone)]
pub(super) struct BenchEnvironment {
    tools: ToolPaths,
    credentials: Option<(String, String)>,
    mirror: Option<String>,
    startup_timeout: std::time::Duration,
}

pub static BENCHMARK_SUITE: &dyn BenchmarkSuite = &SUITE;

impl BenchEnvironment {
    fn from_process() -> Self {
        Self::new(
            None,
            std::env::var("DOCKERHUB_USERNAME")
                .ok()
                .filter(|user| !user.is_empty())
                .zip(std::env::var("DOCKERHUB_TOKEN").ok().filter(|token| !token.is_empty())),
        )
    }

    fn new(directory: Option<&Path>, credentials: Option<(String, String)>) -> Self {
        Self {
            tools: ToolPaths::from_directory(directory),
            credentials,
            mirror: None,
            startup_timeout: std::time::Duration::from_secs(30),
        }
    }

    fn with_mirror(&self, mirror: String) -> Self {
        Self {
            tools: self.tools.clone(),
            credentials: self.credentials.clone(),
            mirror: Some(mirror),
            startup_timeout: self.startup_timeout,
        }
    }
}

#[derive(Clone)]
struct ToolPaths {
    crane: PathBuf,
    docker: PathBuf,
    zot: PathBuf,
}

impl ToolPaths {
    fn from_directory(directory: Option<&Path>) -> Self {
        let path = |name: &str| directory.map_or_else(|| name.into(), |directory| directory.join(name));
        Self {
            crane: path("crane"),
            docker: path("docker"),
            zot: path("zot"),
        }
    }
}

#[async_trait::async_trait]
impl BenchmarkSuite for OciBenchmarkSuite {
    fn name(&self) -> &'static str {
        "oci"
    }

    fn configure(&self, command: Command) -> Command {
        command.arg(
            Arg::new(MIRROR_ARG)
                .long("mirror")
                .help("Use a local pull-through mirror")
                .action(ArgAction::SetTrue),
        )
    }

    async fn run(&self, run: BenchmarkRun<'_>) -> anyhow::Result<()> {
        run_suite(
            BenchEnvironment::from_process(),
            run.context,
            run.matches.get_flag(MIRROR_ARG),
            run.rounds,
            run.skip,
            run.only,
            run.http,
        )
        .await
    }
}

/// Run the OCI suite: every workload not in `skip`, against every registry named in `only`.
///
/// Parts, any of which `--skip` leaves out by name: `pull`, `throughput`, `parallel`, `endpoints`. Set
/// `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN` to pull authenticated (a higher rate ceiling than the
/// anonymous 100/hour); every proxy and crane pick them up.
///
/// With `mirror`, a local pull-through cache stands in for Docker Hub so a many-round run never
/// exhausts the hourly pull ceiling and every server is shielded from upstream network variance. It
/// prices proxy overhead rather than a real Docker Hub fetch, so it is the reproducible-serving
/// variant; the default run talks to Docker Hub directly and should keep `rounds` small.
///
/// # Errors
/// Returns an error when a registry cannot start or a workload against a healthy one fails.
async fn run_suite(
    environment: BenchEnvironment,
    context: &peryx_bench_core::context::BenchmarkContext,
    mirror: bool,
    rounds: usize,
    skip: &[String],
    only: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    let enabled = |part: &str| !skip.iter().any(|skipped| skipped.eq_ignore_ascii_case(part));
    let has_work = ["pull", "throughput", "parallel", "endpoints"].into_iter().any(enabled);
    let mirror = if mirror && has_work {
        Some(servers::start_mirror(&environment).await?)
    } else {
        None
    };
    let environment = mirror.as_ref().map_or_else(
        || environment.clone(),
        |mirror| environment.with_mirror(mirror.url().to_owned()),
    );
    if has_work {
        servers::login_crane(&environment)?;
    }
    let servers: Vec<_> = servers::all()
        .into_iter()
        .filter(|server| only.is_empty() || only.split(',').any(|name| name == server.name))
        .collect();
    anyhow::ensure!(
        !has_work || !servers.is_empty(),
        "no OCI benchmark server matches --only={only}"
    );
    if enabled("pull") {
        workloads::pulls(&environment, context, &servers, rounds).await?;
    }
    if enabled("throughput") {
        workloads::throughput(&environment, context, &servers, rounds, http).await?;
    }
    if enabled("parallel") {
        workloads::fleet(&environment, context, &servers, rounds).await?;
    }
    if enabled("endpoints") {
        workloads::endpoints(&environment, context, &servers, rounds, http).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/bench/tests.rs"]
mod tests;
