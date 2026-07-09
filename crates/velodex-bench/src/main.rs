//! Benchmark velodex against direct `PyPI` and competing index servers.
//!
//! Five workloads, each a table in the site's TOML report:
//!
//! - **install**: time `uv pip install` and `pip install` of the top `PyPI` packages through each server, cold (fresh
//!   server state) and warm (the server keeps its cache, the client starts over). This is the number a user feels.
//! - **throughput**: move one large wheel; four clients racing for it cold, then single and eight-way parallel
//!   downloads of it hot.
//! - **parallel installs**: ten venvs install polars at once with separate client caches, like ten CI jobs hitting the
//!   same server, cold and warm.
//! - **metadata**: fetch a batch of PEP 658 metadata siblings cold, then hot, pricing the resolver fast path without
//!   downloading the whole artifact.
//! - **load**: request-level throughput, one user and a concurrent swarm, against each warm server.
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

mod compare;
mod ecosystems;
mod report;
mod servers;
mod stats;
mod usage;

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, bail};
use clap::Parser;

use crate::ecosystems::Ecosystem;
use crate::report::repo_root;

/// Benchmark velodex against direct upstreams and competing index servers.
///
/// Selection is two-axis: `--ecosystem` picks the suite, `--skip` leaves parts of it out. Each suite
/// names its own parts: `PyPI` has install/pip/throughput/parallel/metadata/load, `OCI` has
/// pull/throughput/parallel/load.
#[derive(Parser, Clone)]
struct Cli {
    /// The package ecosystem to benchmark.
    #[arg(long, value_enum, default_value_t = Ecosystem::Pypi)]
    ecosystem: Ecosystem,

    /// Independent rounds per measurement: each restarts the server on empty state, and the round
    /// samples reduce to a median with its spread. Three gives a robust median, and the per-cell
    /// coefficient of variation flags anything still too noisy to trust; raise it for the `ab` mode,
    /// where the single velodex party is cheap and a few more rounds sharpen the regression verdict.
    #[arg(long, default_value_t = 3)]
    rounds: usize,

    /// Leave out parts of the suite by name; repeat for several.
    #[arg(long, value_name = "PART")]
    skip: Vec<String>,

    /// Comma-separated server names to run (default: all).
    #[arg(long, default_value = "")]
    only: String,

    /// OCI only: put a local pull-through cache in front of Docker Hub so a many-round run is shielded
    /// from upstream rate limits and network variance. Reproducible serving numbers, but the cold rows
    /// then price proxy overhead rather than a real Docker Hub fetch. Without it the run talks to
    /// Docker Hub directly and `--rounds` should stay small to respect the hourly ceiling.
    #[arg(long)]
    mirror: bool,

    #[command(subcommand)]
    mode: Option<Mode>,
}

/// The two things the benchmark compares.
#[derive(clap::Subcommand, Clone)]
enum Mode {
    /// velodex against the other servers: run the suite and write the published report. This is the
    /// default when no mode is given.
    VsRest,
    /// velodex now against velodex at a base commit: build both, run each through this same harness,
    /// print the per-metric A/B verdict, and exit non-zero on a regression. Runs velodex-only unless
    /// `--only` names more.
    Ab {
        /// The git ref (commit, tag, or branch) to compare the working tree against.
        base: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder().build()?;
    match cli.mode.clone() {
        Some(Mode::Ab { base }) => ab(&base, &cli, &http).await,
        Some(Mode::VsRest) | None => {
            ensure_velodex_built()?;
            run_suite(&cli, &http).await
        }
    }
}

/// Run the selected ecosystem's suite with the current settings.
async fn run_suite(cli: &Cli, http: &reqwest::Client) -> anyhow::Result<()> {
    match cli.ecosystem {
        Ecosystem::Pypi => ecosystems::pypi::run(cli.rounds, &cli.skip, &cli.only, http).await,
        Ecosystem::Oci => ecosystems::oci::run(cli.rounds, cli.mirror, &cli.skip, &cli.only, http).await,
    }
}

/// Build velodex from `base_ref` in a throwaway git worktree, run the suite once against it and once
/// against the working-tree build, and compare. Both runs go through this harness, so the two sides
/// share the methodology; a base commit's own harness would use different estimators and make the
/// comparison meaningless.
///
/// The two runs are sequential, so slow thermal drift is not fully cancelled; the gate's noise
/// threshold and the per-metric spread guard against reading drift as a regression.
async fn ab(base_ref: &str, cli: &Cli, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut suite = cli.clone();
    if suite.only.is_empty() {
        "velodex".clone_into(&mut suite.only);
    }
    suite.mode = None;
    ensure_velodex_built()?;
    let head_binary = report::velodex_binary();
    let base_binary = build_base(base_ref)?;
    let saved = save_report()?;

    println!("== measuring base ({base_ref}) ==");
    run_with_binary(&base_binary, &suite, http).await?;
    let base_report = report::repo_root().join("target").join("bench-base-report.toml");
    std::fs::copy(report::report_path(), &base_report)?;

    println!("== measuring working tree ==");
    run_with_binary(&head_binary, &suite, http).await?;
    let regressed = compare::against(&base_report)?;

    restore_report(saved)?;
    let _ = std::fs::remove_file(&base_report);
    remove_worktree()?;
    if regressed {
        bail!("velodex regressed against {base_ref}");
    }
    Ok(())
}

/// Run the suite with the velodex party launched from `binary`, clearing the override afterwards.
async fn run_with_binary(binary: &std::path::Path, cli: &Cli, http: &reqwest::Client) -> anyhow::Result<()> {
    report::set_velodex_binary(Some(binary.to_path_buf()));
    let result = run_suite(cli, http).await;
    report::set_velodex_binary(None);
    result
}

/// The worktree path base builds live in.
fn base_worktree() -> PathBuf {
    report::repo_root().join("target").join("bench-base")
}

/// Check `base_ref` out into a worktree and build its velodex, returning the built binary's path.
fn build_base(base_ref: &str) -> anyhow::Result<PathBuf> {
    let worktree = base_worktree();
    remove_worktree()?;
    println!("preparing base worktree at {}", worktree.display());
    run_git(&[
        "worktree",
        "add",
        "--detach",
        "--force",
        &worktree.to_string_lossy(),
        base_ref,
    ])?;
    println!("building velodex ({base_ref})");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "velodex"])
        .current_dir(&worktree)
        .status()
        .context("cargo did not start for the base build")?;
    if !status.success() {
        bail!("base build of {base_ref} failed");
    }
    Ok(worktree.join("target").join("release").join("velodex"))
}

/// Remove the base worktree if one is left over from an earlier run.
fn remove_worktree() -> anyhow::Result<()> {
    let worktree = base_worktree();
    if worktree.exists() {
        run_git(&["worktree", "remove", "--force", &worktree.to_string_lossy()])?;
    }
    Ok(())
}

/// Read the committed report aside so the A/B runs (which overwrite it) can be undone.
fn save_report() -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(report::report_path()) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Put the saved report back after the A/B runs overwrote it.
fn restore_report(saved: Option<String>) -> anyhow::Result<()> {
    match saved {
        Some(contents) => std::fs::write(report::report_path(), contents)?,
        None => {
            let _ = std::fs::remove_file(report::report_path());
        }
    }
    Ok(())
}

fn run_git(args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(report::repo_root())
        .status()
        .context("git did not start")?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

/// Build the release binary before every run so the benchmark always measures the current source, never
/// a stale artifact from an earlier build. Cargo's incremental build makes this a no-op when nothing
/// changed, so it stays a one-command reproduction while keeping A/B comparisons honest.
fn ensure_velodex_built() -> anyhow::Result<()> {
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
