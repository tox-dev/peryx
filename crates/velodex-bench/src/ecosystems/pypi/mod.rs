//! The `PyPI` benchmark suite: the workloads, the competitor servers, and the package fixtures.
//!
//! This mirrors the `velodex-ecosystem-pypi` crate and the site's `content/ecosystems/pypi.md`.

pub mod packages;
pub mod servers;
pub mod workloads;

/// A part of the `PyPI` suite `--skip` can leave out; the second selection axis (`--part`).
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Part {
    /// The install workload.
    Install,
    /// The pip client inside the install workload (uv still runs).
    Pip,
    /// The file throughput workload.
    Throughput,
    /// The parallel-CI install workload.
    Parallel,
    /// The PEP 658 metadata sibling workload.
    Metadata,
    /// The request swarm workload.
    Load,
}

/// Run the `PyPI` suite: every workload not in `skip`, against every server named in `only`.
///
/// # Errors
/// Returns an error when a server cannot start or a workload against a healthy server fails.
pub async fn run(runs: usize, skip: &[Part], only: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    let servers: Vec<_> = servers::all()
        .into_iter()
        .filter(|server| only.is_empty() || only.split(',').any(|name| name == server.name))
        .collect();
    let enabled = |part: Part| !skip.contains(&part);
    if enabled(Part::Install) {
        let clients: &[&str] = if enabled(Part::Pip) { &["uv", "pip"] } else { &["uv"] };
        workloads::installs(&servers, clients, runs, http).await?;
    }
    if enabled(Part::Throughput) {
        workloads::throughput(&servers, http).await?;
    }
    if enabled(Part::Parallel) {
        workloads::fleet(&servers, http).await?;
    }
    if enabled(Part::Metadata) {
        workloads::metadata(&servers, http).await?;
    }
    if enabled(Part::Load) {
        workloads::load(&servers, &[1, 32], http).await?;
    }
    Ok(())
}
