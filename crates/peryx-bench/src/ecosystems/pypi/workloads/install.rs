//! The install workload: every server, cold then warm, per client, over `rounds` restarts.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context as _, bail};

use super::super::packages::TOP_PACKAGES;
use super::{Rounds, report_samples, run_checked};
use crate::report::{Absent, Metric, baseline, cost_rows, network_row, publish, row, summarize, table};
use crate::servers::Server;
use crate::usage::{Cost, Usage};

/// The install workload: every server, cold then warm, per client, over `rounds` restarts.
///
/// # Errors
/// Returns an error when a server cannot start or an install against a healthy server fails.
pub async fn installs(
    servers: &[Server],
    clients: &[&str],
    rounds: usize,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    prewarm_cdn()?;
    for client in clients {
        let mut cold: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
        let mut warm: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
        let mut costs: Vec<Option<Vec<Cost>>> = Vec::new();
        for (index, server) in servers.iter().enumerate() {
            let mut collected = Rounds::new();
            for attempt in 1..=rounds {
                let scratch = tempfile::tempdir()?;
                let state = scratch.path().join("state");
                std::fs::create_dir(&state)?;
                let active = server.start(&state, http).await?;
                let usage = Usage::watch(active.pid());
                println!("[{client}] {} round {attempt}/{rounds}", server.name);
                match install_round(client, &active.url, scratch.path()) {
                    Ok((cold_seconds, warm_seconds)) => {
                        cold[index].push(cold_seconds);
                        warm[index].push(warm_seconds);
                    }
                    Err(error) => println!("[{client}] {} round {attempt}: failed ({error:#})", server.name),
                }
                collected.record_cost(usage);
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
        publish(
            &format!("install-{client}"),
            table(
                &format!("{client}: install the top {} PyPI packages", TOP_PACKAGES.len()),
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
fn install_round(client: &str, index_url: &str, scratch: &Path) -> anyhow::Result<(f64, f64)> {
    let cold = install_once(client, index_url, scratch)?;
    let warm = install_once(client, index_url, scratch)?;
    Ok((cold, warm))
}

/// One unmeasured direct install so `PyPI`'s CDN edge is equally warm for every party.
///
/// Without it the first party measured pays the CDN's cold-cache penalty and everyone after rides
/// the edge cache that run just warmed, biasing the comparison by run order.
fn prewarm_cdn() -> anyhow::Result<()> {
    println!("prewarming the CDN edge (unmeasured)");
    let scratch = tempfile::tempdir()?;
    install_once("uv", "https://pypi.org/simple/", scratch.path())?;
    Ok(())
}

/// Time one from-scratch install of the workload through `index_url`.
fn install_once(client: &str, index_url: &str, scratch: &Path) -> anyhow::Result<f64> {
    let workdir = tempfile::tempdir_in(scratch)?;
    let venv = workdir.path().join("venv");
    run_checked(Command::new("uv").args(["venv"]).arg(&venv))?;
    let mut command;
    if client == "uv" {
        command = Command::new("uv");
        command
            .args(["pip", "install", "--index-url", index_url])
            .args(TOP_PACKAGES)
            .env("VIRTUAL_ENV", &venv)
            .env("UV_CACHE_DIR", workdir.path().join("client-cache"));
    } else {
        run_checked(
            Command::new("uv")
                .args(["pip", "install", "--python"])
                .arg(venv.join("bin").join("python"))
                .arg("pip"),
        )?;
        command = Command::new(venv.join("bin").join("pip"));
        command
            .args(["install", "--no-cache-dir", "--disable-pip-version-check"])
            .args(["--index-url", index_url])
            .args(TOP_PACKAGES);
    }
    let start = Instant::now();
    let output = command.output().context("install client did not start")?;
    let elapsed = start.elapsed().as_secs_f64();
    if !output.status.success() {
        bail!(
            "install via {index_url} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(elapsed)
}
