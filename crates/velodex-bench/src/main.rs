//! Benchmark velodex against direct `PyPI` and competing index servers.
//!
//! Five workloads, each a table in the site's TOML report:
//!
//! - **install**: time `uv pip install` and `pip install` of the top `PyPI` packages through each
//!   server, cold (fresh server state) and warm (the server keeps its cache, the client starts
//!   over). This is the number a user feels.
//! - **throughput**: move one large wheel; four clients racing for it cold, then single and
//!   eight-way parallel downloads of it hot.
//! - **parallel installs**: ten venvs install polars at once with separate client caches, like ten
//!   CI jobs hitting the same server, cold and warm.
//! - **metadata**: fetch a batch of PEP 658 metadata siblings cold, then hot, pricing the resolver
//!   fast path without downloading the whole artifact.
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

mod ecosystems;
mod report;
mod servers;
mod usage;

use std::process::Command;

use anyhow::{Context as _, bail};
use clap::Parser;

use crate::ecosystems::Ecosystem;
use crate::ecosystems::pypi::Part;
use crate::report::repo_root;

/// Benchmark velodex against direct `PyPI` and competing index servers.
///
/// Selection is two-axis: `--ecosystem` picks the suite, `--skip` leaves parts of it out.
#[derive(Parser)]
struct Cli {
    /// The package ecosystem to benchmark.
    #[arg(long, value_enum, default_value_t = Ecosystem::Pypi)]
    ecosystem: Ecosystem,

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
    let http = reqwest::Client::builder().build()?;
    match cli.ecosystem {
        Ecosystem::Pypi => ecosystems::pypi::run(cli.runs, &cli.skip, &cli.only, &http).await,
    }
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
