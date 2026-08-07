use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, bail};
use clap::Parser;

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
            ensure_peryx_built()?;
            run_suite(&cli, &http).await
        }
    }
}

/// Run the selected ecosystem's suite with the current settings.
///
/// The host profile comes first, before any server is up: it is ecosystem-neutral, and its loopback
/// number is the ceiling the serving rows below are read against.
async fn run_suite(cli: &Cli, http: &reqwest::Client) -> anyhow::Result<()> {
    if !cli.skip.iter().any(|part| part == "machine") {
        machine::publish().await?;
    }
    match cli.ecosystem {
        Ecosystem::Pypi => peryx_ecosystem_pypi::bench::run(&cli.pypi, cli.rounds, &cli.skip, &cli.only, http).await,
        Ecosystem::Oci => peryx_ecosystem_oci::bench::run(&cli.oci, cli.rounds, &cli.skip, &cli.only, http).await,
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
    ensure_peryx_built()?;
    let head_binary = report::peryx_binary();
    let base_binary = build_base(base_ref)?;
    let saved = save_report()?;

    let base_report = report::repo_root().join("target").join("bench-base-report.toml");
    let head_report = report::repo_root().join("target").join("bench-head-report.toml");
    if head_first {
        measure("working tree", &head_binary, &head_report, &suite, http).await?;
        measure(&format!("base ({base_ref})"), &base_binary, &base_report, &suite, http).await?;
        std::fs::copy(&head_report, report::report_path())?;
    } else {
        measure(&format!("base ({base_ref})"), &base_binary, &base_report, &suite, http).await?;
        measure("working tree", &head_binary, &head_report, &suite, http).await?;
    }
    let regressed = compare::against(&base_report)?;

    restore_report(saved)?;
    for report in [base_report, head_report] {
        let _ = std::fs::remove_file(report);
    }
    remove_worktree()?;
    if regressed {
        bail!("peryx regressed against {base_ref}");
    }
    Ok(())
}

async fn measure(
    label: &str,
    binary: &std::path::Path,
    destination: &std::path::Path,
    cli: &Cli,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    println!("== measuring {label} ==");
    run_with_binary(binary, cli, http).await?;
    std::fs::copy(report::report_path(), destination)?;
    Ok(())
}

/// Run the suite with the peryx party launched from `binary`, clearing the override afterwards.
async fn run_with_binary(binary: &std::path::Path, cli: &Cli, http: &reqwest::Client) -> anyhow::Result<()> {
    report::set_peryx_binary(Some(binary.to_path_buf()));
    let result = run_suite(cli, http).await;
    report::set_peryx_binary(None);
    result
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
fn ensure_peryx_built() -> anyhow::Result<()> {
    println!("building peryx (release)");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "peryx"])
        .current_dir(repo_root())
        .status()
        .context("cargo did not start")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    Ok(())
}
