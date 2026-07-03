//! The four workloads: installs, file throughput, a parallel CI fleet, and a request swarm.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use crate::packages::{FLEET_PACKAGE, STRESS_PROJECT, TOP_PACKAGES};
use crate::report::{Absent, Metric, Row, publish, robust_mean, row, table};
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
        let mut walls: Vec<(Option<f64>, Option<f64>)> = Vec::new();
        let mut costs: Vec<Option<Cost>> = Vec::new();
        for server in servers {
            let mut cold_samples = Vec::new();
            let mut warm_samples = Vec::new();
            let mut cost: Option<Cost> = None;
            for attempt in 1..=runs {
                let scratch = tempfile::tempdir()?;
                let state = scratch.path().join("state");
                std::fs::create_dir(&state)?;
                let active = server.start(&state, http).await?;
                let usage = Usage::watch(active.pid());
                println!("[{client}] {} #{attempt}: cold", server.name);
                cold_samples.push(install_once(client, &active.url, scratch.path())?);
                println!("[{client}] {} #{attempt}: warm", server.name);
                warm_samples.push(install_once(client, &active.url, scratch.path())?);
                cost = usage.finish().or(cost);
            }
            let cold = robust_mean(&mut cold_samples);
            let warm = robust_mean(&mut warm_samples);
            println!("[{client}] {}: cold {cold:.1}s warm {warm:.1}s", server.name);
            walls.push((Some(cold), Some(warm)));
            costs.push(cost);
        }
        let base = baseline(servers);
        let mut rows = vec![
            row(
                "cold cache",
                &walls.iter().map(|&(cold, _)| cold).collect::<Vec<_>>(),
                base,
                Metric::Seconds,
                Absent::Failed,
            ),
            row(
                "warm cache",
                &walls.iter().map(|&(_, warm)| warm).collect::<Vec<_>>(),
                base,
                Metric::Seconds,
                Absent::Failed,
            ),
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
    let mut measured: Vec<Option<(f64, f64, f64)>> = Vec::new();
    let mut costs: Vec<Option<Cost>> = Vec::new();
    for server in servers {
        let mut cold_samples = Vec::new();
        let mut hot1_samples = Vec::new();
        let mut hot8_samples = Vec::new();
        let mut cost: Option<Cost> = None;
        let mut failed = false;
        for _ in 0..runs {
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            let usage = Usage::watch(active.pid());
            // A server erroring under contention is itself a result worth a table cell.
            let outcome = transfer_series(&active, &filename, http).await;
            cost = usage.finish().or(cost);
            match outcome {
                Ok((cold4, hot1, hot8)) => {
                    cold_samples.push(cold4);
                    hot1_samples.push(hot1);
                    hot8_samples.push(hot8);
                }
                Err(error) => {
                    println!("[throughput] {}: failed under contention ({error:#})", server.name);
                    failed = true;
                    break;
                }
            }
        }
        if failed || cold_samples.is_empty() {
            measured.push(None);
        } else {
            let cold4 = robust_mean(&mut cold_samples);
            let hot1 = robust_mean(&mut hot1_samples);
            let hot8 = robust_mean(&mut hot8_samples);
            println!(
                "[throughput] {}: cold-4 {cold4:.1}s, hot {hot1:.0} MB/s, hot-8 {hot8:.0} MB/s",
                server.name
            );
            measured.push(Some((cold4, hot1, hot8)));
        }
        costs.push(cost);
    }
    let base = baseline(servers);
    let mut rows = vec![
        row(
            "cold cache: 4 clients, one wheel",
            &measured
                .iter()
                .map(|cells| cells.map(|(cold4, ..)| cold4))
                .collect::<Vec<_>>(),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "hot cache: single download",
            &measured
                .iter()
                .map(|cells| cells.map(|(_, hot1, _)| hot1))
                .collect::<Vec<_>>(),
            base,
            Metric::Rate("MB/s"),
            Absent::Failed,
        ),
        row(
            "hot cache: 8 parallel downloads",
            &measured
                .iter()
                .map(|cells| cells.map(|(.., hot8)| hot8))
                .collect::<Vec<_>>(),
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
    let mut walls: Vec<Option<(f64, f64)>> = Vec::new();
    let mut costs: Vec<Option<Cost>> = Vec::new();
    for server in servers {
        let mut cold_samples = Vec::new();
        let mut warm_samples = Vec::new();
        let mut cost: Option<Cost> = None;
        let mut failed = false;
        for _ in 0..runs {
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
            cost = usage.finish().or(cost);
            match outcome {
                Ok((cold, warm)) => {
                    cold_samples.push(cold);
                    warm_samples.push(warm);
                }
                Err(error) => {
                    println!("[fleet] {}: failed under the fleet ({error:#})", server.name);
                    failed = true;
                    break;
                }
            }
        }
        if failed || cold_samples.is_empty() {
            walls.push(None);
        } else {
            let cold = robust_mean(&mut cold_samples);
            let warm = robust_mean(&mut warm_samples);
            let spent = cost.map_or_else(
                || "no server".to_owned(),
                |cost| {
                    format!(
                        "{:.1}s CPU, {} MB peak",
                        cost.cpu_seconds,
                        cost.peak_rss_bytes / 1_000_000
                    )
                },
            );
            println!("[fleet] {}: cold {cold:.1}s warm {warm:.1}s, {spent}", server.name);
            walls.push(Some((cold, warm)));
        }
        costs.push(cost);
    }
    let base = baseline(servers);
    let mut rows = vec![
        row(
            "cold cache: 10 parallel installs",
            &walls
                .iter()
                .map(|walls| walls.map(|(cold, _)| cold))
                .collect::<Vec<_>>(),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "warm cache: 10 parallel installs",
            &walls
                .iter()
                .map(|walls| walls.map(|(_, warm)| warm))
                .collect::<Vec<_>>(),
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

/// The request workload: a swarm of resolvers fetching project pages against each warm server.
///
/// # Errors
/// Returns an error when a server cannot start or its pages cannot be warmed.
pub async fn load(servers: &[Server], users: &[usize], runs: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut measured: Vec<Vec<Swarm>> = Vec::new();
    let mut costs: Vec<Option<Cost>> = Vec::new();
    for server in servers {
        let scratch = tempfile::tempdir()?;
        let state = scratch.path().join("state");
        std::fs::create_dir(&state)?;
        let active = server.start(&state, http).await?;
        warm_pages(&active.url, http).await?;
        let usage = Usage::watch(active.pid());
        let mut per_user = Vec::new();
        for &count in users {
            println!("[load] {}: {count} user(s)", server.name);
            // Trimmed means across windows, metric by metric; the request total stays a sum so
            // the CPU-per-thousand ratio spans everything the server actually answered.
            let windows: Vec<Swarm> = {
                let mut collected = Vec::new();
                for _ in 0..runs {
                    collected.push(swarm(&active.url, count).await?);
                }
                collected
            };
            per_user.push(Swarm {
                rps: robust_mean(&mut windows.iter().map(|window| window.rps).collect::<Vec<_>>()),
                p95: robust_mean(&mut windows.iter().map(|window| window.p95).collect::<Vec<_>>()),
                p99: robust_mean(&mut windows.iter().map(|window| window.p99).collect::<Vec<_>>()),
                requests: windows.iter().map(|window| window.requests).sum(),
            });
        }
        costs.push(usage.finish());
        measured.push(per_user);
    }
    let base = baseline(servers);
    let mut rows = Vec::new();
    for (index, &count) in users.iter().enumerate() {
        let label = if count == 1 {
            "1 user".to_owned()
        } else {
            format!("{count} users")
        };
        rows.push(row(
            &format!("{label}: requests/s"),
            &measured.iter().map(|user| Some(user[index].rps)).collect::<Vec<_>>(),
            base,
            Metric::Rate("req/s"),
            Absent::Failed,
        ));
        rows.push(row(
            &format!("{label}: p95 latency"),
            &measured.iter().map(|user| Some(user[index].p95)).collect::<Vec<_>>(),
            base,
            Metric::Seconds,
            Absent::Failed,
        ));
        rows.push(row(
            &format!("{label}: p99 latency"),
            &measured.iter().map(|user| Some(user[index].p99)).collect::<Vec<_>>(),
            base,
            Metric::Seconds,
            Absent::Failed,
        ));
    }
    // Raw CPU seconds would reward doing nothing: the fast servers answer an order of magnitude
    // more requests inside the fixed window, so the cost row normalizes per thousand answered.
    let anchor = anchor(servers);
    #[expect(clippy::cast_precision_loss, reason = "request counts fit f64 exactly here")]
    let per_thousand: Vec<Option<f64>> = costs
        .iter()
        .zip(&measured)
        .map(|(cost, swarms)| {
            let requests: usize = swarms.iter().map(|swarm| swarm.requests).sum();
            cost.map(|cost| cost.cpu_seconds / (requests as f64 / 1000.0))
        })
        .collect();
    rows.push(row(
        "server CPU per 1,000 requests",
        &per_thousand,
        anchor,
        Metric::Seconds,
        Absent::NoServer,
    ));
    #[expect(clippy::cast_precision_loss, reason = "resident sizes fit f64 to the byte")]
    let rss: Vec<Option<f64>> = costs
        .iter()
        .map(|cost| cost.map(|cost| cost.peak_rss_bytes as f64 / 1e6))
        .collect();
    rows.push(row(
        "server peak memory",
        &rss,
        anchor,
        Metric::Amount("MB"),
        Absent::NoServer,
    ));
    publish(
        "load",
        table("simple-page requests against a warm cache", servers, base, rows),
    )
}

/// One swarm window's outcome.
struct Swarm {
    rps: f64,
    p95: f64,
    p99: f64,
    requests: usize,
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

/// Drive `users` concurrent resolver-style clients for 20 seconds.
async fn swarm(index_url: &str, users: usize) -> anyhow::Result<Swarm> {
    const WINDOW: Duration = Duration::from_secs(20);
    let mut tasks = Vec::new();
    for user in 0..users {
        let index_url = index_url.to_owned();
        tasks.push(tokio::spawn(async move {
            // Each user keeps its own connections, like one resolver process would.
            let client = reqwest::Client::builder().build().expect("client builds");
            let deadline = Instant::now() + WINDOW;
            let mut latencies = Vec::new();
            let mut page = user;
            while Instant::now() < deadline {
                let target = format!("{index_url}{}/", TOP_PACKAGES[page % 10]);
                page += 1;
                let start = Instant::now();
                let outcome = async {
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
                if outcome.is_ok() {
                    latencies.push(start.elapsed().as_secs_f64());
                }
            }
            latencies
        }));
    }
    let mut latencies = Vec::new();
    for task in tasks {
        latencies.extend(task.await.expect("swarm user never panics"));
    }
    if latencies.is_empty() {
        bail!("the swarm completed no requests");
    }
    latencies.sort_unstable_by(f64::total_cmp);
    #[expect(clippy::cast_precision_loss, reason = "request counts fit f64 exactly here")]
    let rps = latencies.len() as f64 / WINDOW.as_secs_f64();
    let quantile = |per_cent: usize| latencies[(latencies.len() * per_cent / 100).min(latencies.len() - 1)];
    Ok(Swarm {
        rps,
        p95: quantile(95),
        p99: quantile(99),
        requests: latencies.len(),
    })
}
