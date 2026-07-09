//! The `PyPI` benchmark suite: the workloads, the competitor servers, and the package fixtures.
//!
//! This mirrors the `peryx-ecosystem-pypi` crate and the site's `content/ecosystems/pypi.md`.

pub mod packages;
pub mod servers;
pub mod workloads;

/// Run the `PyPI` suite: every workload not in `skip`, against every server named in `only`.
///
/// Parts, any of which `--skip` leaves out by name: `install`, `pip` (the pip client inside the
/// install workload; uv still runs), `throughput`, `parallel`, `metadata`, `load`.
///
/// # Errors
/// Returns an error when a server cannot start or a workload against a healthy server fails.
pub async fn run(rounds: usize, skip: &[String], only: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    let servers: Vec<_> = servers::all()
        .into_iter()
        .filter(|server| only.is_empty() || only.split(',').any(|name| name == server.name))
        .collect();
    let enabled = |part: &str| !skip.iter().any(|skipped| skipped.eq_ignore_ascii_case(part));
    if enabled("install") {
        let clients: &[&str] = if enabled("pip") { &["uv", "pip"] } else { &["uv"] };
        workloads::installs(&servers, clients, rounds, http).await?;
    }
    if enabled("throughput") {
        workloads::throughput(&servers, rounds, http).await?;
    }
    if enabled("parallel") {
        workloads::fleet(&servers, rounds, http).await?;
    }
    if enabled("metadata") {
        workloads::metadata(&servers, rounds, http).await?;
    }
    if enabled("load") {
        workloads::load(&servers, &[1, 32], rounds, http).await?;
    }
    Ok(())
}
