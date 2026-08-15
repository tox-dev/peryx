use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context as _, bail};

use super::super::packages::TOP_PACKAGES;
use super::{BENCH_PYTHON, Rounds, report_samples, run_checked};
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::report::{Absent, Metric, baseline, cost_rows, network_row, row, summarize, table};
use peryx_bench_core::servers::Server;
use peryx_bench_core::usage::{Cost, Usage};

/// The install workload: every server, cold then warm, per client, over `rounds` restarts.
///
/// # Errors
/// Returns an error when a server cannot start or an install against a healthy server fails.
pub async fn installs(
    context: &BenchmarkContext,
    servers: &[Server],
    clients: &[&str],
    rounds: usize,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    installs_packages(
        context,
        servers,
        clients,
        rounds,
        http,
        InstallInput {
            packages: TOP_PACKAGES,
            python: BENCH_PYTHON,
            prewarm_index: "https://pypi.org/simple/",
        },
    )
    .await
}

struct InstallInput<'a> {
    packages: &'a [&'a str],
    python: &'a str,
    prewarm_index: &'a str,
}

async fn installs_packages(
    context: &BenchmarkContext,
    servers: &[Server],
    clients: &[&str],
    rounds: usize,
    http: &reqwest::Client,
    input: InstallInput<'_>,
) -> anyhow::Result<()> {
    if rounds > 0 && !clients.is_empty() && !servers.is_empty() {
        prewarm_cdn(context.scratch(), input.prewarm_index, input.packages, input.python)?;
    }
    for client in clients {
        let mut cold: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
        let mut warm: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
        let mut costs: Vec<Option<Vec<Cost>>> = Vec::new();
        for (index, server) in servers.iter().enumerate() {
            let mut collected = Rounds::new();
            for attempt in 1..=rounds {
                let scratch = tempfile::tempdir_in(context.scratch())?;
                let state = scratch.path().join("state");
                std::fs::create_dir(&state)?;
                let active = server.start(context, &state, http).await?;
                let usage = Usage::watch(active.pid())?;
                println!("[{client}] {} round {attempt}/{rounds}", server.name);
                match install_round(client, &active.url, scratch.path(), input.packages, input.python) {
                    Ok((cold_seconds, warm_seconds)) => {
                        cold[index].push(cold_seconds);
                        warm[index].push(warm_seconds);
                    }
                    Err(error) => println!("[{client}] {} round {attempt}: failed ({error:#})", server.name),
                }
                collected.record_cost(usage)?;
            }
            report_samples(&format!("[{client}] {}", server.name), &cold[index], &warm[index]);
            costs.push(collected.costs());
        }
        let base = baseline(servers);
        let mut rows = vec![
            network_row("cold cache", &summarize(&cold), base, Metric::Seconds, Absent::Failed),
            row("warm cache", &summarize(&warm), base, Metric::Seconds, Absent::Failed),
        ];
        rows.extend(cost_rows(servers, &costs));
        context.publish(
            &format!("install-{client}"),
            table(
                &format!("{client}: install the top {} PyPI packages", input.packages.len()),
                servers,
                base,
                rows,
            ),
        )?;
    }
    Ok(())
}

/// One install round: a cold install (empty cache) then a warm one (the server keeps its cache, the
/// client starts over). Fallible as a unit so a flaky server becomes an error cell, not a run abort.
fn install_round(
    client: &str,
    index_url: &str,
    scratch: &Path,
    packages: &[&str],
    python: &str,
) -> anyhow::Result<(f64, f64)> {
    let cold = install_once(client, index_url, scratch, packages, python)?;
    let warm = install_once(client, index_url, scratch, packages, python)?;
    Ok((cold, warm))
}

/// One unmeasured direct install so `PyPI`'s CDN edge is equally warm for every party.
///
/// Without it the first party measured pays the CDN's cold-cache penalty and everyone after rides
/// the edge cache that run just warmed, biasing the comparison by run order.
fn prewarm_cdn(scratch: &Path, index_url: &str, packages: &[&str], python: &str) -> anyhow::Result<()> {
    println!("prewarming the CDN edge (unmeasured)");
    let directory = tempfile::tempdir_in(scratch)?;
    install_once("uv", index_url, directory.path(), packages, python)?;
    Ok(())
}

/// Time one from-scratch install of the workload through `index_url`.
fn install_once(client: &str, index_url: &str, scratch: &Path, packages: &[&str], python: &str) -> anyhow::Result<f64> {
    let workdir = tempfile::tempdir_in(scratch)?;
    let venv = workdir.path().join("venv");
    run_checked(Command::new("uv").args(["venv", "--python", python]).arg(&venv))?;
    let (setup, install) = install_plan(client, index_url, packages, &venv, workdir.path());
    run_install_plan(index_url, setup, install)
}

fn install_plan(
    client: &str,
    index_url: &str,
    packages: &[&str],
    venv: &Path,
    workdir: &Path,
) -> (Vec<Command>, Command) {
    if client == "uv" {
        let mut command = Command::new("uv");
        command
            .args(["pip", "install", "--index-url", index_url])
            .args(["--only-binary", ":all:"])
            .args(packages)
            .env("VIRTUAL_ENV", venv)
            .env("UV_CACHE_DIR", workdir.join("client-cache"));
        (Vec::new(), command)
    } else {
        let mut setup = Command::new("uv");
        setup
            .args(["pip", "install", "--python"])
            .arg(venv.join("bin").join("python"))
            .arg("pip");
        let mut command = Command::new(venv.join("bin").join("pip"));
        command
            .args(["install", "--no-cache-dir", "--disable-pip-version-check"])
            .args(["--only-binary", ":all:"])
            .args(["--index-url", index_url])
            .args(packages);
        (vec![setup], command)
    }
}

fn run_install_plan(index_url: &str, mut setup: Vec<Command>, mut install: Command) -> anyhow::Result<f64> {
    for command in &mut setup {
        run_checked(command)?;
    }
    let start = Instant::now();
    let output = install.output().context("install client did not start")?;
    let elapsed = start.elapsed().as_secs_f64();
    if !output.status.success() {
        bail!(
            "install via {index_url} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(elapsed)
}

#[cfg(test)]
#[path = "../../../tests/unit/bench/workloads/install.rs"]
mod tests;
