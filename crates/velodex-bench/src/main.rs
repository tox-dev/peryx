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
//! - **load**: peak sustainable throughput, found by ramping concurrency against each warm server
//!   until throughput tops out, reporting both request rate and megabytes of page served (direct is
//!   excluded: pypi.org is a CDN, not a comparable server).
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
    /// The peak-throughput workload.
    Load,
}

/// Benchmark velodex against direct `PyPI` and competing index servers.
#[derive(Parser)]
struct Cli {
    /// Samples per cell: extremes are dropped and the rest averaged.
    #[arg(long, default_value_t = 5)]
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
    scratch_on_a_roomy_volume();
    ensure_velodex_built()?;
    let servers: Vec<_> = servers::all()
        .into_iter()
        .filter(|server| cli.only.is_empty() || cli.only.split(',').any(|name| name == server.name))
        .collect();
    report::publish_meta(&manifest())?;
    let http = reqwest::Client::builder().build()?;
    let runs = |part: Part| !cli.skip.contains(&part);
    if runs(Part::Install) {
        let clients: &[&str] = if runs(Part::Pip) { &["uv", "pip"] } else { &["uv"] };
        workloads::installs(&servers, clients, cli.runs, &http).await?;
    }
    if runs(Part::Throughput) {
        workloads::throughput(&servers, cli.runs, &http).await?;
    }
    if runs(Part::Parallel) {
        workloads::fleet(&servers, cli.runs, &http).await?;
    }
    if runs(Part::Load) {
        workloads::load(&servers, cli.runs, &http).await?;
    }
    Ok(())
}

/// Point the harness's scratch at the repository's own volume unless the caller set `TMPDIR`. The
/// fleet workload fans ten package caches out at once, and the system temp is often a small
/// partition a run overflows. Every client cache the workloads create lives under a temp dir, so
/// redirecting the temp root moves the heavy writes off the system partition with it.
fn scratch_on_a_roomy_volume() {
    if std::env::var_os("TMPDIR").is_some() {
        return;
    }
    let scratch = repo_root().join("target").join("bench-scratch");
    if std::fs::create_dir_all(&scratch).is_ok() {
        let _ = tempfile::env::override_temp_dir(&scratch);
    }
}

/// The run's machine and toolchain, so a reader can judge and reproduce the numbers: absolute
/// figures move with the hardware and the client versions, and only a stated platform makes a
/// benchmark trustworthy.
fn manifest() -> Vec<(&'static str, String)> {
    use sysinfo::System;
    let mut system = System::new();
    system.refresh_cpu_all();
    system.refresh_memory();
    let cpu = system
        .cpus()
        .first()
        .map_or_else(|| "unknown".to_owned(), |cpu| cpu.brand().trim().to_owned());
    let os = System::long_os_version().unwrap_or_else(|| "unknown".to_owned());
    let memory_gib = format!("{} GiB", system.total_memory() / (1 << 30));
    let tool_version = |command: &str, args: &[&str]| {
        std::process::Command::new(command)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map_or_else(
                || "unknown".to_owned(),
                |output| String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            )
    };
    vec![
        ("os", os),
        ("cpu", cpu),
        ("logical_cores", system.cpus().len().to_string()),
        ("memory", memory_gib),
        // The uplink caps every server that reaches upstream, so it bounds the cold-install and
        // redirect numbers directly; no probe reports it, so it is stated for the reference machine
        // and wants editing when the suite runs elsewhere.
        ("connection", "1 Gbit symmetric fiber, Los Angeles".to_owned()),
        ("uv", tool_version("uv", &["--version"])),
        ("python", tool_version("python3", &["--version"])),
        ("date", tool_version("date", &["-u", "+%Y-%m-%d"])),
    ]
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
