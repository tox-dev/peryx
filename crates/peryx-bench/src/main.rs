use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, bail};
use clap::Parser;

use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::report::repo_root;
use peryx_bench_core::{compare, machine, report};

#[derive(clap::ValueEnum, Clone, Copy)]
enum Ecosystem {
    Pypi,
    Oci,
}

/// Benchmark peryx against direct upstreams and competing index servers.
#[derive(Parser, Clone)]
struct Cli {
    /// The ecosystem suite to benchmark.
    #[arg(long, value_enum, default_value_t = Ecosystem::Pypi)]
    ecosystem: Ecosystem,

    /// Independent rounds per measurement: each restarts the server on empty state, and the round
    /// samples reduce to a median with its spread. Three supplies a median, and the per-cell
    /// coefficient of variation flags anything still too noisy to trust; raise it for the `ab` mode,
    /// where the single peryx party is cheap and a few more rounds sharpen the regression verdict.
    #[arg(long, default_value_t = 3)]
    rounds: usize,

    /// Leave out parts of the suite by name; repeat for several.
    #[arg(long, value_name = "PART")]
    skip: Vec<String>,

    /// Comma-separated server names to run (default: all).
    #[arg(long, default_value = "")]
    only: String,

    #[command(subcommand)]
    mode: Option<Mode>,

    #[command(flatten)]
    pypi: peryx_ecosystem_pypi::bench::Options,

    #[command(flatten)]
    oci: peryx_ecosystem_oci::bench::Options,
}

/// The two things the benchmark compares.
#[derive(clap::Subcommand, Clone)]
enum Mode {
    /// peryx against the other servers: run the suite and write the published report. This is the
    /// default when no mode is given.
    VsRest,
    /// peryx now against peryx at a base commit: build both, run each through this same harness,
    /// print the per-metric A/B verdict, and exit non-zero on a regression. Runs peryx-only unless
    /// `--only` names more.
    Ab {
        /// The git ref (commit, tag, or branch) to compare the working tree against.
        base: String,
        /// Measure the working tree before the base, so a second run can expose order-dependent drift.
        #[arg(long)]
        head_first: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder().build()?;
    match cli.mode.clone() {
        Some(Mode::Ab { base, head_first }) => ab(&base, head_first, &cli, &http).await,
        Some(Mode::VsRest) | None => {
            let context = BenchmarkContext::workspace(ensure_peryx_built()?);
            run_suite(&context, &cli, &http).await
        }
    }
}

/// Run the selected ecosystem's suite with the current settings.
///
/// The host profile comes first, before any server is up: it is ecosystem-neutral, and its loopback
/// number is the ceiling the serving rows below are read against.
async fn run_suite(context: &BenchmarkContext, cli: &Cli, http: &reqwest::Client) -> anyhow::Result<()> {
    if !cli.skip.iter().any(|part| part == "machine") {
        machine::publish().await?;
    }
    match cli.ecosystem {
        Ecosystem::Pypi => {
            peryx_ecosystem_pypi::bench::run(context, &cli.pypi, cli.rounds, &cli.skip, &cli.only, http).await
        }
        Ecosystem::Oci => {
            peryx_ecosystem_oci::bench::run(context, &cli.oci, cli.rounds, &cli.skip, &cli.only, http).await
        }
    }
}

/// Build peryx from `base_ref` in a throwaway git worktree, run the suite once against it and once
/// against the working-tree build, and compare. Both runs go through this harness, so the two sides
/// share the methodology; a base commit's own harness would use different estimators and make the
/// comparison meaningless.
///
/// The two runs are sequential, so slow thermal drift is not fully cancelled. `head_first` reverses
/// their order for a confirming run; the gate also rejects noisy metrics.
async fn ab(base_ref: &str, head_first: bool, cli: &Cli, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut suite = cli.clone();
    if suite.only.is_empty() {
        "peryx".clone_into(&mut suite.only);
    }
    if !suite.skip.iter().any(|part| part == "machine") {
        suite.skip.push("machine".to_owned());
    }
    suite.mode = None;
    let head_binary = ensure_peryx_built()?;
    let base_binary = build_base(base_ref)?;

    let base_report = report::repo_root().join("target").join("bench-base-report.toml");
    let head_report = report::repo_root().join("target").join("bench-head-report.toml");
    let base_context = BenchmarkContext::new(base_binary, base_report.clone());
    let head_context = BenchmarkContext::new(head_binary, head_report.clone());
    if head_first {
        measure("working tree", &head_context, &suite, http).await?;
        measure(&format!("base ({base_ref})"), &base_context, &suite, http).await?;
    } else {
        measure(&format!("base ({base_ref})"), &base_context, &suite, http).await?;
        measure("working tree", &head_context, &suite, http).await?;
    }
    let regressed = compare::against_paths(&base_report, &head_report)?;

    for report in [base_report, head_report] {
        let _ = std::fs::remove_file(report);
    }
    remove_worktree()?;
    if regressed {
        bail!("peryx regressed against {base_ref}");
    }
    Ok(())
}

async fn measure(label: &str, context: &BenchmarkContext, cli: &Cli, http: &reqwest::Client) -> anyhow::Result<()> {
    println!("== measuring {label} ==");
    run_suite(context, cli, http).await
}

/// The worktree path base builds live in.
fn base_worktree() -> PathBuf {
    report::repo_root().join("target").join("bench-base")
}

/// Check `base_ref` out into a worktree and build its peryx, returning the built binary's path.
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
    println!("building peryx ({base_ref})");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "peryx"])
        .arg("--target-dir")
        .arg(worktree.join("target"))
        .current_dir(&worktree)
        .status()
        .context("cargo did not start for the base build")?;
    if !status.success() {
        bail!("base build of {base_ref} failed");
    }
    Ok(worktree.join("target").join("release").join("peryx"))
}

/// Remove the base worktree if one is left over from an earlier run.
fn remove_worktree() -> anyhow::Result<()> {
    let worktree = base_worktree();
    if worktree.exists() {
        run_git(&["worktree", "remove", "--force", &worktree.to_string_lossy()])?;
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

/// Build before every run so a stale binary cannot affect a comparison.
fn ensure_peryx_built() -> anyhow::Result<PathBuf> {
    println!("building peryx (release)");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "peryx"])
        .current_dir(repo_root())
        .status()
        .context("cargo did not start")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(repo_root())
        .output()
        .context("cargo metadata did not start")?;
    if !output.status.success() {
        bail!("cargo metadata failed");
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let target = metadata["target_directory"]
        .as_str()
        .context("cargo metadata omitted target_directory")?;
    Ok(PathBuf::from(target)
        .join("release")
        .join(format!("peryx{}", std::env::consts::EXE_SUFFIX)))
}
