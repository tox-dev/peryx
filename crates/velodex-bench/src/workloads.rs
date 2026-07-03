//! The four workloads: installs, file throughput, a parallel CI fleet, and a request swarm.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use crate::packages::{FLEET_PACKAGE, STRESS_PROJECT, TOP_PACKAGES};
use crate::report::{Absent, Metric, Row, publish, robust_mean, row, row_samples, table};
use crate::servers::{Active, Server};
use crate::usage::{Cost, Usage};

/// The index of the no-proxy baseline party, `direct`.
fn baseline(servers: &[Server]) -> usize {
    servers.iter().position(|server| server.name == "direct").unwrap_or(0)
}

/// The party resource rows compare against: direct runs no server, so it cannot anchor them.
fn anchor(servers: &[Server]) -> usize {
    servers
        .iter()
        .position(|server| server.name == "velodex")
        .unwrap_or_else(|| baseline(servers))
}

/// The rows every table ends with: what the server itself burned while the workload ran.
fn cost_rows(servers: &[Server], costs: &[Option<Cost>]) -> Vec<Row> {
    let anchor = anchor(servers);
    let cpu: Vec<Option<f64>> = costs.iter().map(|cost| cost.map(|cost| cost.cpu_seconds)).collect();
    #[expect(clippy::cast_precision_loss, reason = "resident sizes fit f64 to the byte")]
    let rss: Vec<Option<f64>> = costs
        .iter()
        .map(|cost| cost.map(|cost| cost.peak_rss_bytes as f64 / 1e6))
        .collect();
    vec![
        row("server CPU", &cpu, anchor, Metric::Seconds, Absent::NoServer),
        row(
            "server peak memory",
            &rss,
            anchor,
            Metric::Amount("MB"),
            Absent::NoServer,
        ),
    ]
}

/// The install workload: every server, cold then warm, per client; best of `runs`.
///
/// # Errors
/// Returns an error when a server cannot start or an install against a healthy server fails.
pub async fn installs(servers: &[Server], clients: &[&str], runs: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    prewarm_cdn()?;
    for client in clients {
        let mut cold: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
        let mut warm: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
        let mut costs: Vec<Option<Cost>> = vec![None; servers.len()];
        // Interleave server order round by round rather than finishing one server before the next,
        // so a drift in the network or the laptop's thermal state spreads across every party
        // instead of penalizing whoever the run reached last.
        for round in 1..=runs {
            for (index, server) in servers.iter().enumerate() {
                let scratch = tempfile::tempdir()?;
                let state = scratch.path().join("state");
                std::fs::create_dir(&state)?;
                let active = server.start(&state, http).await?;
                let usage = Usage::watch(active.pid());
                println!("[{client}] {} round {round}: cold", server.name);
                cold[index].push(install_once(client, &active.url, scratch.path())?);
                println!("[{client}] {} round {round}: warm", server.name);
                warm[index].push(install_once(client, &active.url, scratch.path())?);
                costs[index] = usage.finish().or_else(|| costs[index].take());
            }
        }
        let base = baseline(servers);
        let cold_cells: Vec<Option<Vec<f64>>> = cold.into_iter().map(Some).collect();
        let warm_cells: Vec<Option<Vec<f64>>> = warm.into_iter().map(Some).collect();
        let mut rows = vec![
            row_samples("cold cache", &cold_cells, base, Metric::Seconds, Absent::Failed),
            row_samples("warm cache", &warm_cells, base, Metric::Seconds, Absent::Failed),
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

fn run_checked(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output().context("command did not start")?;
    if !output.status.success() {
        bail!("{command:?} failed:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

/// The file-transfer workload: one large wheel, cold under contention and hot at full speed.
///
/// The cold row sends four clients after the same uncached wheel at once, which is what a CI fleet
/// does to a cache the moment a new release lands: it measures whether the server fans one
/// upstream transfer out to every waiter or serializes them. The hot rows measure how fast a
/// cached wheel leaves the server, alone and under eight parallel readers.
///
/// # Errors
/// Returns an error when a server cannot start; a server failing the transfers is a table cell.
pub async fn throughput(servers: &[Server], runs: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let filename = stress_wheel_filename(http).await?;
    println!("[throughput] measuring with {filename}");
    let mut cold: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut hot1: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut hot8: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut costs: Vec<Option<Cost>> = vec![None; servers.len()];
    let mut failed = vec![false; servers.len()];
    // Interleave the parties round by round so drift spreads evenly (see the install workload).
    for _ in 0..runs {
        for (index, server) in servers.iter().enumerate() {
            if failed[index] {
                continue;
            }
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            let usage = Usage::watch(active.pid());
            // A server erroring under contention is itself a result worth a table cell.
            let outcome = transfer_series(&active, &filename, http).await;
            costs[index] = usage.finish().or_else(|| costs[index].take());
            match outcome {
                Ok((cold4, single, eight)) => {
                    cold[index].push(cold4);
                    hot1[index].push(single);
                    hot8[index].push(eight);
                }
                Err(error) => {
                    println!("[throughput] {}: failed under contention ({error:#})", server.name);
                    failed[index] = true;
                }
            }
        }
    }
    let base = baseline(servers);
    let cells = |data: Vec<Vec<f64>>, fail: &[bool]| -> Vec<Option<Vec<f64>>> {
        data.into_iter()
            .enumerate()
            .map(|(index, samples)| (!fail[index] && !samples.is_empty()).then_some(samples))
            .collect()
    };
    let mut rows = vec![
        row_samples(
            "cold cache: 4 clients, one wheel",
            &cells(cold, &failed),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row_samples(
            "hot cache: single download",
            &cells(hot1, &failed),
            base,
            Metric::Rate("MB/s"),
            Absent::Failed,
        ),
        row_samples(
            "hot cache: 8 parallel downloads",
            &cells(hot8, &failed),
            base,
            Metric::Rate("MB/s"),
            Absent::Failed,
        ),
    ];
    rows.extend(cost_rows(servers, &costs));
    publish(
        "throughput",
        table(
            &format!("moving one large wheel ({STRESS_PROJECT}): cold under contention, hot at speed"),
            servers,
            base,
            rows,
        ),
    )
}

async fn transfer_series(active: &Active, filename: &str, http: &reqwest::Client) -> anyhow::Result<(f64, f64, f64)> {
    let url = wheel_url(&active.url, STRESS_PROJECT, filename, http).await?;
    let cold4 = parallel_downloads(&url, 4, http).await?;
    // Hot transfers are sub-second syscall benchmarks; three in-session samples feed the outer
    // trimmed mean. The cold transfer cannot repeat without resetting server state.
    let mut singles = Vec::new();
    let mut size = 0;
    for _ in 0..3 {
        let (seconds, bytes) = timed_download(&url, http).await?;
        singles.push(seconds);
        size = bytes;
    }
    let single = robust_mean(&mut singles);
    let mut hot8_walls = Vec::new();
    for _ in 0..3 {
        hot8_walls.push(parallel_downloads(&url, 8, http).await?);
    }
    let hot8_wall = robust_mean(&mut hot8_walls);
    #[expect(clippy::cast_precision_loss, reason = "wheel sizes fit f64 to the byte")]
    Ok((cold4, size as f64 / single / 1e6, 8.0 * size as f64 / hot8_wall / 1e6))
}

/// The concrete wheel every server moves, resolved once from `PyPI` so all parties match.
async fn stress_wheel_filename(http: &reqwest::Client) -> anyhow::Result<String> {
    let body = http
        .get(format!("https://pypi.org/simple/{STRESS_PROJECT}/"))
        .header("Accept", "application/vnd.pypi.simple.v1+json")
        .send()
        .await?
        .text()
        .await?;
    let page: serde_json::Value = serde_json::from_str(&body)?;
    let tags: &[&str] = if cfg!(target_os = "macos") {
        &["macosx", "arm64"]
    } else {
        &["manylinux", "x86_64"]
    };
    page["files"]
        .as_array()
        .context("simple JSON has no files")?
        .iter()
        .filter_map(|file| file["filename"].as_str())
        .rfind(|name| tags.iter().all(|tag| name.contains(tag)))
        .map(str::to_owned)
        .context("no wheel matches this platform")
}

/// Resolve `filename`'s download URL through a server's simple page, JSON or HTML alike.
async fn wheel_url(index_url: &str, project: &str, filename: &str, http: &reqwest::Client) -> anyhow::Result<String> {
    let response = http
        .get(format!("{index_url}{project}/"))
        .header("Accept", "application/vnd.pypi.simple.v1+json, text/html;q=0.5")
        .send()
        .await?
        .error_for_status()?;
    let page_url = response.url().clone();
    let json_page = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/vnd.pypi.simple.v1+json"));
    let body = response.text().await?;
    let href = if json_page {
        let page: serde_json::Value = serde_json::from_str(&body)?;
        page["files"]
            .as_array()
            .context("simple JSON has no files")?
            .iter()
            .find(|file| file["filename"].as_str() == Some(filename))
            .and_then(|file| file["url"].as_str())
            .context("wheel missing from the JSON page")?
            .to_owned()
    } else {
        html_href(&body, filename).context("wheel missing from the HTML page")?
    };
    let absolute = page_url.join(href.split('#').next().unwrap_or(&href))?;
    Ok(absolute.into())
}

/// The first `href="…"` on the page whose target mentions `filename`; no HTML parser needed for
/// the anchor-list pages every simple index serves.
fn html_href(body: &str, filename: &str) -> Option<String> {
    body.split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .find(|target| target.contains(filename))
        .map(str::to_owned)
}

/// One full download; returns wall seconds and byte count.
async fn timed_download(url: &str, http: &reqwest::Client) -> anyhow::Result<(f64, u64)> {
    let start = Instant::now();
    let mut response = http.get(url).send().await?.error_for_status()?;
    let mut total = 0u64;
    while let Some(chunk) = response.chunk().await? {
        total += chunk.len() as u64;
    }
    Ok((start.elapsed().as_secs_f64(), total))
}

/// `clients` simultaneous downloads of the same URL; returns wall seconds until all finish.
async fn parallel_downloads(url: &str, clients: usize, http: &reqwest::Client) -> anyhow::Result<f64> {
    let start = Instant::now();
    let downloads: Vec<_> = (0..clients)
        .map(|_| {
            let url = url.to_owned();
            let http = http.clone();
            tokio::spawn(async move { timed_download(&url, &http).await })
        })
        .collect();
    for download in downloads {
        download.await.expect("download task never panics")?;
    }
    Ok(start.elapsed().as_secs_f64())
}

/// The CI-fleet workload: ten venvs install polars at once, cold then warm.
///
/// Each worker gets its own empty uv cache, exactly like ten CI jobs landing on the same runner
/// pool: the server sees ten simultaneous copies of every page and wheel request.
///
/// # Errors
/// Returns an error when a server cannot start; a server failing the fleet is a table cell.
pub async fn fleet(servers: &[Server], runs: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut cold: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut warm: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut costs: Vec<Option<Cost>> = vec![None; servers.len()];
    let mut failed = vec![false; servers.len()];
    // Interleave the parties round by round so drift spreads evenly (see the install workload).
    for _ in 0..runs {
        for (index, server) in servers.iter().enumerate() {
            if failed[index] {
                continue;
            }
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            let usage = Usage::watch(active.pid());
            // A server erroring under the fleet is itself a result worth a table cell.
            let outcome = match fleet_install(&active.url, scratch.path(), 10) {
                Ok(cold) => fleet_install(&active.url, scratch.path(), 10).map(|warm| (cold, warm)),
                Err(error) => Err(error),
            };
            costs[index] = usage.finish().or_else(|| costs[index].take());
            match outcome {
                Ok((cold_wall, warm_wall)) => {
                    cold[index].push(cold_wall);
                    warm[index].push(warm_wall);
                }
                Err(error) => {
                    println!("[fleet] {}: failed under the fleet ({error:#})", server.name);
                    failed[index] = true;
                }
            }
        }
    }
    let base = baseline(servers);
    let cells = |data: Vec<Vec<f64>>, fail: &[bool]| -> Vec<Option<Vec<f64>>> {
        data.into_iter()
            .enumerate()
            .map(|(index, samples)| (!fail[index] && !samples.is_empty()).then_some(samples))
            .collect()
    };
    let mut rows = vec![
        row_samples(
            "cold cache: 10 parallel installs",
            &cells(cold, &failed),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row_samples(
            "warm cache: 10 parallel installs",
            &cells(warm, &failed),
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

/// The request workload: find the peak sustainable request throughput of each warm server.
///
/// Rather than latency at one fixed arrival rate, this ramps concurrency until throughput stops
/// climbing (or the server starts erroring) and reports the highest rate it retired — the honest
/// "how much can it push" number. Each server saturates at its own concurrency, so a fragile one
/// reports the ceiling it holds rather than the collapse a heavier pool would force on it.
///
/// `direct` is left out: pypi.org is a CDN over the open internet, so its ceiling measures the
/// uplink, not a comparable server, and a throughput ramp has no business hammering a public index.
///
/// # Errors
/// Returns an error when a server cannot start or its pages cannot be warmed.
pub async fn load(servers: &[Server], runs: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    const MEASURE: Duration = Duration::from_secs(10);
    let mut peaks: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut mbps: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut p95: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut p99: Vec<Vec<f64>> = vec![Vec::new(); servers.len()];
    let mut at: Vec<Option<f64>> = vec![None; servers.len()];
    let mut answered: Vec<usize> = vec![0; servers.len()];
    let mut costs: Vec<Option<Cost>> = vec![None; servers.len()];
    for (index, server) in servers.iter().enumerate() {
        if server.name == "direct" {
            continue;
        }
        let scratch = tempfile::tempdir()?;
        let state = scratch.path().join("state");
        std::fs::create_dir(&state)?;
        let active = server.start(&state, http).await?;
        warm_pages(&active.url, http).await?;
        let connections = peak(&active.url).await;
        #[expect(clippy::cast_precision_loss, reason = "connection counts are tiny")]
        let connections_f64 = connections as f64;
        at[index] = Some(connections_f64);
        println!("[load] {}: peak at {connections} connections", server.name);
        let usage = Usage::watch(active.pid());
        for _ in 0..runs {
            let window = saturate(&active.url, connections, MEASURE).await;
            peaks[index].push(window.rps);
            mbps[index].push(window.mbps);
            p95[index].push(window.p95);
            p99[index].push(window.p99);
            answered[index] += window.requests;
        }
        costs[index] = usage.finish();
    }
    // direct runs no server, so velodex anchors both the ratios and the resource rows here.
    let base = anchor(servers);
    let samples = |column: &[Vec<f64>]| -> Vec<Option<Vec<f64>>> {
        column
            .iter()
            .map(|values| (!values.is_empty()).then(|| values.clone()))
            .collect()
    };
    let mut rows = vec![
        row_samples(
            "peak req/s",
            &samples(&peaks),
            base,
            Metric::Rate("req/s"),
            Absent::NoServer,
        ),
        row_samples(
            "data served",
            &samples(&mbps),
            base,
            Metric::Rate("MB/s"),
            Absent::NoServer,
        ),
        row_samples(
            "p95 latency at peak",
            &samples(&p95),
            base,
            Metric::Seconds,
            Absent::NoServer,
        ),
        row_samples(
            "p99 latency at peak",
            &samples(&p99),
            base,
            Metric::Seconds,
            Absent::NoServer,
        ),
        row(
            "connections at peak",
            &at,
            base,
            Metric::Amount("conns"),
            Absent::NoServer,
        ),
    ];
    rows.extend(request_cost_rows(&costs, &answered, base));
    publish(
        "load",
        table("peak sustainable request throughput", servers, base, rows),
    )
}

/// The resource rows the request table ends with: CPU normalized per thousand requests answered (raw
/// seconds would reward a server that redirects everything and does almost nothing) and the peak
/// resident memory of the server's process tree.
fn request_cost_rows(costs: &[Option<Cost>], answered: &[usize], base: usize) -> Vec<Row> {
    #[expect(clippy::cast_precision_loss, reason = "request counts fit f64 exactly here")]
    let per_thousand: Vec<Option<f64>> = costs
        .iter()
        .zip(answered)
        .map(|(cost, &requests)| {
            cost.filter(|_| requests > 0)
                .map(|cost| cost.cpu_seconds / (requests as f64 / 1000.0))
        })
        .collect();
    #[expect(clippy::cast_precision_loss, reason = "resident sizes fit f64 to the byte")]
    let rss: Vec<Option<f64>> = costs
        .iter()
        .map(|cost| cost.map(|cost| cost.peak_rss_bytes as f64 / 1e6))
        .collect();
    vec![
        row(
            "server CPU per 1,000 requests",
            &per_thousand,
            base,
            Metric::Seconds,
            Absent::NoServer,
        ),
        row("server peak memory", &rss, base, Metric::Amount("MB"), Absent::NoServer),
    ]
}

async fn warm_pages(index_url: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    for package in &TOP_PACKAGES[..10] {
        http.get(format!("{index_url}{package}/"))
            .header("Accept", "*/*")
            .send()
            .await?
            .error_for_status()?;
    }
    Ok(())
}

/// One saturation window's outcome at a fixed concurrency.
struct Window {
    rps: f64,
    mbps: f64,
    p95: f64,
    p99: f64,
    requests: usize,
    errors: usize,
}

/// The concurrency levels the ramp probes, in order. It rides through run-to-run noise and stops
/// only when a server is plainly past its knee, so a fragile server settles at the ceiling it holds
/// and a fast one climbs until the client itself becomes the bottleneck.
const RAMP: &[usize] = &[1, 2, 4, 8, 16, 24, 32, 48, 64];

/// Ramp concurrency and return the level that retired the most requests per second. A short probe
/// window at each rung keeps the sweep quick; the caller then measures that point properly. The ramp
/// keeps the best rung seen (not the last), so a single noisy dip cannot end it early — it stops only
/// once the server starts erroring or throughput has collapsed to half its best.
async fn peak(index_url: &str) -> usize {
    const PROBE: Duration = Duration::from_secs(6);
    let mut best_rps = 0.0;
    let mut best = RAMP[0];
    for &connections in RAMP {
        let window = saturate(index_url, connections, PROBE).await;
        if window.rps > best_rps {
            best_rps = window.rps;
            best = connections;
        }
        let total = window.requests + window.errors;
        #[expect(clippy::cast_precision_loss, reason = "request counts fit f64 exactly here")]
        let error_rate = if total == 0 {
            1.0
        } else {
            window.errors as f64 / total as f64
        };
        if error_rate > 0.1 || window.rps < best_rps * 0.5 {
            break;
        }
    }
    best
}

/// Drive `connections` clients flat out against the index for a fixed window and report both how
/// many requests per second the server retired and how many megabytes of page it delivered.
///
/// The client behaves the way uv and pip do — it keeps connections alive and follows redirects — so
/// the numbers translate to what a real resolver sees against each server. That is what a redirecting
/// server like pypiserver costs honestly: its page requests follow a 303 to pypi.org, so its data
/// throughput is bounded by the upstream and the uplink rather than by a local store, while a server
/// that caches the page serves it at local speed. Closed-loop by design (each client sends its next
/// request the instant the last returns), since peak throughput asks how fast a server empties its
/// queue under maximum push. Latency is the round-trip of successful requests; a per-request timeout
/// and a whole-window cap keep a server that stalls or deadlocks from hanging the run.
async fn saturate(index_url: &str, connections: usize, window: Duration) -> Window {
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
    let start = Instant::now();
    let deadline = start + window;
    let next = Arc::new(AtomicU64::new(0));
    // Each sample is (round-trip latency, bytes delivered) for one successful request; a failure
    // records no sample, so the counts below credit only what the client actually received.
    let samples: Arc<std::sync::Mutex<Vec<(f64, usize)>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let attempts = Arc::new(AtomicU64::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..connections {
        let index_url = index_url.to_owned();
        let next = next.clone();
        let samples = samples.clone();
        let attempts = attempts.clone();
        tasks.spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("client builds");
            while Instant::now() < deadline {
                let issued = next.fetch_add(1, Ordering::Relaxed);
                let target = format!(
                    "{index_url}{}/",
                    TOP_PACKAGES[usize::try_from(issued).unwrap_or(0) % 10]
                );
                let sent = Instant::now();
                let served = async {
                    client
                        .get(&target)
                        .header("Accept", "*/*")
                        .send()
                        .await?
                        .error_for_status()?
                        .bytes()
                        .await
                }
                .await;
                attempts.fetch_add(1, Ordering::Relaxed);
                if let Ok(body) = served {
                    samples
                        .lock()
                        .expect("samples lock")
                        .push((sent.elapsed().as_secs_f64(), body.len()));
                }
            }
        });
    }
    // A server can wedge a connection past its request timeout; cap the whole window so one that
    // deadlocks reads as the low throughput and censored tail it earned instead of hanging the run.
    let cap = window + REQUEST_TIMEOUT + Duration::from_secs(3);
    let drain = async { while tasks.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(cap, drain).await;
    tasks.abort_all();
    let mut samples = std::mem::take(&mut *samples.lock().expect("samples lock"));
    let successes = samples.len();
    let bytes: usize = samples.iter().map(|&(_, len)| len).sum();
    samples.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
    #[expect(clippy::cast_precision_loss, reason = "counts and byte totals fit f64 here")]
    let (rps, mbps) = (
        successes as f64 / window.as_secs_f64(),
        bytes as f64 / 1e6 / window.as_secs_f64(),
    );
    let quantile = |per_cent: usize| {
        samples
            .get((samples.len() * per_cent / 100).min(samples.len().saturating_sub(1)))
            .map_or(0.0, |&(latency, _)| latency)
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "attempt counts stay well under usize::MAX"
    )]
    let errors = (attempts.load(Ordering::Relaxed) as usize).saturating_sub(successes);
    Window {
        rps,
        mbps,
        p95: quantile(95),
        p99: quantile(99),
        requests: successes,
        errors,
    }
}
