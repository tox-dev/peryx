//! The CI-fleet workload: ten venvs install polars at once, cold then warm, over `rounds` restarts.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context as _, bail};

use super::super::packages::FLEET_PACKAGE;
use super::{Rounds, report_samples, run_checked};
use crate::report::{Absent, Metric, baseline, cost_rows, network_row, publish, row, summarize, table};
use crate::servers::Server;
use crate::usage::{Cost, Usage};

/// The CI-fleet workload: ten venvs install polars at once, cold then warm, over `rounds` restarts.
///
/// Each worker gets its own empty uv cache, exactly like ten CI jobs landing on the same runner pool:
/// the server sees ten simultaneous copies of every page and wheel request.
///
/// # Errors
/// Returns an error when a server cannot start; a server failing the fleet is a table cell.
pub async fn fleet(servers: &[Server], rounds: usize, http: &reqwest::Client) -> anyhow::Result<()> {
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
            match fleet_round(&active.url, scratch.path()) {
                Ok((cold_seconds, warm_seconds)) => {
                    cold[index].push(cold_seconds);
                    warm[index].push(warm_seconds);
                }
                Err(error) => println!("[fleet] {} round {attempt}: failed ({error:#})", server.name),
            }
            collected.record_cost(usage);
        }
        report_samples(&format!("[fleet] {}", server.name), &cold[index], &warm[index]);
        costs.push(collected.costs());
    }
    let base = baseline(servers);
    let mut rows = vec![
        network_row(
            "cold cache: 10 parallel installs",
            &summarize(&cold),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "warm cache: 10 parallel installs",
            &summarize(&warm),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
    ];
    rows.extend(cost_rows(servers, &costs));
    publish(
        "parallel-install",
        table(
            &format!("uv: ten venvs install {FLEET_PACKAGE} at once"),
            servers,
            base,
            rows,
        ),
    )
}

/// One round of the fleet workload: ten cold installs, then ten warm ones against the same server.
fn fleet_round(index_url: &str, scratch: &Path) -> anyhow::Result<(f64, f64)> {
    let cold = fleet_install(index_url, scratch, 10)?;
    let warm = fleet_install(index_url, scratch, 10)?;
    Ok((cold, warm))
}

/// Install the fleet package into `workers` fresh venvs at once; returns wall seconds.
fn fleet_install(index_url: &str, scratch: &Path, workers: usize) -> anyhow::Result<f64> {
    let rundir = tempfile::tempdir_in(scratch)?;
    let venvs: Vec<_> = (0..workers)
        .map(|index| rundir.path().join(format!("venv-{index}")))
        .collect();
    for venv in &venvs {
        run_checked(Command::new("uv").arg("venv").arg(venv))?;
    }
    let start = Instant::now();
    let threads: Vec<_> = venvs
        .iter()
        .map(|venv| {
            let venv = venv.clone();
            let index_url = index_url.to_owned();
            std::thread::spawn(move || {
                let output = Command::new("uv")
                    .args(["pip", "install", "--index-url", &index_url, FLEET_PACKAGE])
                    .env("VIRTUAL_ENV", &venv)
                    .env("UV_CACHE_DIR", format!("{}-cache", venv.display()))
                    .output()
                    .context("uv did not start")?;
                if !output.status.success() {
                    bail!(
                        "fleet install via {index_url} failed:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Ok(())
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("fleet worker never panics")?;
    }
    Ok(start.elapsed().as_secs_f64())
}
