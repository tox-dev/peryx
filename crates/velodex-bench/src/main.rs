//! Benchmark velodex against direct `PyPI` and competing index servers.
//!
//! Four workloads, each a table in the site's TOML report:
//!
//! - **install**: time `uv pip install` and `pip install` of the top `PyPI` packages through each
//!   server, cold (fresh server state) and warm (the server keeps its cache, the client starts
//!   over). This is the number a user feels.
//! - **throughput**: move one large wheel; four clients racing for it cold, then single and
//!   eight-way parallel downloads of it hot.
//! - **parallel installs**: ten venvs install polars at once with separate client caches, like ten
//!   CI jobs hitting the same server, cold and warm.
//! - **load**: request-level throughput, one user and a concurrent swarm, against each warm
//!   server.
//!
//! Every table also reports what the server itself burned while its workload ran: CPU seconds and
//! peak resident memory across the whole process tree. Results land in
//! `site/data/bench/report.toml`; the documentation renders them as tinted tables (best-in-row
//! green to worst-in-row red) via the `bench` shortcode. One command reproduces every table
//! (velodex is built automatically when the release binary is missing):
//!
//! ```shell
//! cargo run --release -p velodex-bench
//! ```

mod packages;
mod report;
mod servers;
mod usage;
mod workloads;

use std::process::Command;

use anyhow::{Context as _, bail};
use clap::Parser;

use crate::report::repo_root;

/// A part of the suite `--skip` can leave out.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Part {
    /// The install workload.
    Install,
    /// The pip client inside the install workload (uv still runs).
    Pip,
    /// The file throughput workload.
    Throughput,
    /// The parallel-CI install workload.
    Parallel,
    /// The request swarm workload.
    Load,
}

/// Benchmark velodex against direct `PyPI` and competing index servers.
#[derive(Parser)]
struct Cli {
    /// Measurements per install cell; the best is kept.
    #[arg(long, default_value_t = 1)]
    runs: usize,

    /// Leave out parts of the suite; repeat for several.
    #[arg(long, value_enum)]
    skip: Vec<Part>,

    /// Comma-separated server names to run (default: all).
    #[arg(long, default_value = "")]
    only: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _ = rustls::crypto::ring::default_provider().install_default();
    ensure_velodex_built()?;
    let servers: Vec<_> = servers::all()
        .into_iter()
        .filter(|server| cli.only.is_empty() || cli.only.split(',').any(|name| name == server.name))
        .collect();
    let http = reqwest::Client::builder().build()?;
    let runs = |part: Part| !cli.skip.contains(&part);
    if runs(Part::Install) {
        let clients: &[&str] = if runs(Part::Pip) { &["uv", "pip"] } else { &["uv"] };
        workloads::installs(&servers, clients, cli.runs, &http).await?;
    }
    if runs(Part::Throughput) {
        workloads::throughput(&servers, &http).await?;
    }
    if runs(Part::Parallel) {
        workloads::fleet(&servers, &http).await?;
    }
    if runs(Part::Load) {
        workloads::load(&servers, &[1, 32], &http).await?;
    }
    Ok(())
}

/// Build the release binary when it is absent, so one command reproduces everything.
fn ensure_velodex_built() -> anyhow::Result<()> {
    if repo_root().join("target").join("release").join("velodex").exists() {
        return Ok(());
    }
    println!("building velodex (release)");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "velodex"])
        .current_dir(repo_root())
        .status()
        .context("cargo did not start")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    Ok(())
}
