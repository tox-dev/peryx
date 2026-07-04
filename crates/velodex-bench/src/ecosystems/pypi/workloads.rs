//! The four workloads: installs, file throughput, a parallel CI fleet, and a request swarm.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use super::packages::{FLEET_PACKAGE, METADATA_PROJECT, STRESS_PROJECT, TOP_PACKAGES};
use crate::report::{Absent, Metric, Row, publish, row, table};
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
            let mut cold = f64::INFINITY;
            let mut warm = f64::INFINITY;
            let mut cost: Option<Cost> = None;
            for attempt in 1..=runs {
                let scratch = tempfile::tempdir()?;
                let state = scratch.path().join("state");
                std::fs::create_dir(&state)?;
                let active = server.start(&state, http).await?;
                let usage = Usage::watch(active.pid());
                println!("[{client}] {} #{attempt}: cold", server.name);
                cold = cold.min(install_once(client, &active.url, scratch.path())?);
                println!("[{client}] {} #{attempt}: warm", server.name);
                warm = warm.min(install_once(client, &active.url, scratch.path())?);
                cost = usage.finish().or(cost);
            }
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
pub async fn throughput(servers: &[Server], http: &reqwest::Client) -> anyhow::Result<()> {
    let filename = stress_wheel_filename(http).await?;
    println!("[throughput] measuring with {filename}");
    let mut measured: Vec<Option<(f64, f64, f64)>> = Vec::new();
    let mut costs: Vec<Option<Cost>> = Vec::new();
    for server in servers {
        let scratch = tempfile::tempdir()?;
        let state = scratch.path().join("state");
        std::fs::create_dir(&state)?;
        let active = server.start(&state, http).await?;
        let usage = Usage::watch(active.pid());
        // A server erroring under contention is itself a result worth a table cell.
        let outcome = transfer_series(&active, &filename, http).await;
        costs.push(usage.finish());
        match outcome {
            Ok((cold4, hot1, hot8)) => {
                println!(
                    "[throughput] {}: cold-4 {cold4:.1}s, hot {hot1:.0} MB/s, hot-8 {hot8:.0} MB/s",
                    server.name
                );
                measured.push(Some((cold4, hot1, hot8)));
            }
            Err(error) => {
                println!("[throughput] {}: failed under contention ({error:#})", server.name);
                measured.push(None);
            }
        }
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
    // Hot transfers are sub-second syscall benchmarks; the best of three smooths scheduler noise.
    // The cold transfer cannot repeat without resetting server state.
    let mut single = f64::INFINITY;
    let mut size = 0;
    for _ in 0..3 {
        let (seconds, bytes) = timed_download(&url, http).await?;
        single = single.min(seconds);
        size = bytes;
    }
    let mut hot8_wall = f64::INFINITY;
    for _ in 0..3 {
        hot8_wall = hot8_wall.min(parallel_downloads(&url, 8, http).await?);
    }
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
pub async fn fleet(servers: &[Server], http: &reqwest::Client) -> anyhow::Result<()> {
    let mut walls: Vec<Option<(f64, f64)>> = Vec::new();
    let mut costs: Vec<Option<Cost>> = Vec::new();
    for server in servers {
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
        let cost = usage.finish();
        match outcome {
            Ok((cold, warm)) => {
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
            Err(error) => {
                println!("[fleet] {}: failed under the fleet ({error:#})", server.name);
                walls.push(None);
            }
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

/// The metadata workload: resolve a batch of PEP 658 siblings, cold then hot.
///
/// # Errors
/// Returns an error when a server cannot start; a server failing the metadata requests is a table
/// cell.
pub async fn metadata(servers: &[Server], http: &reqwest::Client) -> anyhow::Result<()> {
    let mut measured: Vec<Option<(f64, f64, f64)>> = Vec::new();
    let mut costs: Vec<Option<Cost>> = Vec::new();
    for server in servers {
        let scratch = tempfile::tempdir()?;
        let state = scratch.path().join("state");
        std::fs::create_dir(&state)?;
        let active = server.start(&state, http).await?;
        let usage = Usage::watch(active.pid());
        let outcome = metadata_series(&active.url, http).await;
        costs.push(usage.finish());
        match outcome {
            Ok((cold, hot, rate)) => {
                println!(
                    "[metadata] {}: cold {cold:.1}s, hot {hot:.1}s, hot {rate:.0} documents/s",
                    server.name
                );
                measured.push(Some((cold, hot, rate)));
            }
            Err(error) => {
                println!("[metadata] {}: failed ({error:#})", server.name);
                measured.push(None);
            }
        }
    }
    let base = baseline(servers);
    let mut rows = vec![
        row(
            "cold metadata siblings",
            &measured
                .iter()
                .map(|cells| cells.map(|(cold, ..)| cold))
                .collect::<Vec<_>>(),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "hot metadata siblings",
            &measured
                .iter()
                .map(|cells| cells.map(|(_, hot, _)| hot))
                .collect::<Vec<_>>(),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "hot metadata throughput",
            &measured
                .iter()
                .map(|cells| cells.map(|(.., rate)| rate))
                .collect::<Vec<_>>(),
            base,
            Metric::Rate("docs/s"),
            Absent::Failed,
        ),
    ];
    rows.extend(cost_rows(servers, &costs));
    publish(
        "metadata",
        table(
            &format!("PEP 658 metadata siblings for {METADATA_PROJECT}"),
            servers,
            base,
            rows,
        ),
    )
}

async fn metadata_series(index_url: &str, http: &reqwest::Client) -> anyhow::Result<(f64, f64, f64)> {
    let urls = metadata_urls(index_url, http).await?;
    let cold = timed_metadata_batch(&urls, http).await?;
    let mut hot = f64::INFINITY;
    for _ in 0..3 {
        hot = hot.min(timed_metadata_batch(&urls, http).await?);
    }
    #[expect(clippy::cast_precision_loss, reason = "metadata batch lengths fit f64 exactly")]
    Ok((cold, hot, urls.len() as f64 / hot))
}

async fn metadata_urls(index_url: &str, http: &reqwest::Client) -> anyhow::Result<Vec<String>> {
    const LIMIT: usize = 16;
    let response = http
        .get(format!("{index_url}{METADATA_PROJECT}/"))
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
    let urls = if json_page {
        json_metadata_urls(&page_url, &body)?
    } else {
        html_metadata_urls(&page_url, &body)?
    };
    let urls = urls.into_iter().take(LIMIT).collect::<Vec<_>>();
    if urls.is_empty() {
        bail!("{METADATA_PROJECT} exposes no PEP 658 metadata URLs");
    }
    Ok(urls)
}

fn json_metadata_urls(page_url: &url::Url, body: &str) -> anyhow::Result<Vec<String>> {
    let page: serde_json::Value = serde_json::from_str(body)?;
    page["files"]
        .as_array()
        .context("simple JSON has no files")?
        .iter()
        .filter(|file| file["filename"].as_str().is_some_and(is_wheel_path))
        .filter(|file| metadata_present(&file["core-metadata"]))
        .filter_map(|file| file["url"].as_str())
        .map(|href| page_url.join(&format!("{}.metadata", href.split('#').next().unwrap_or(href))))
        .map(|result| result.map(|url| url.to_string()).map_err(Into::into))
        .collect()
}

fn is_wheel_path(path: &str) -> bool {
    let without_fragment = path.split('#').next().unwrap_or(path);
    let without_query = without_fragment.split('?').next().unwrap_or(without_fragment);
    std::path::Path::new(without_query)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("whl"))
}

const fn metadata_present(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(present) => *present,
        serde_json::Value::Object(_) => true,
        _ => false,
    }
}

fn html_metadata_urls(page_url: &url::Url, body: &str) -> anyhow::Result<Vec<String>> {
    body.split("<a ")
        .filter(|anchor| anchor.contains("data-core-metadata"))
        .filter_map(|anchor| anchor.split("href=\"").nth(1)?.split('"').next())
        .filter(|href| is_wheel_path(href))
        .map(|href| page_url.join(&format!("{}.metadata", href.split('#').next().unwrap_or(href))))
        .map(|result| result.map(|url| url.to_string()).map_err(Into::into))
        .collect()
}

async fn timed_metadata_batch(urls: &[String], http: &reqwest::Client) -> anyhow::Result<f64> {
    let start = Instant::now();
    for url in urls {
        http.get(url).send().await?.error_for_status()?.bytes().await?;
    }
    Ok(start.elapsed().as_secs_f64())
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
pub async fn load(servers: &[Server], users: &[usize], http: &reqwest::Client) -> anyhow::Result<()> {
    let mut measured: Vec<Vec<(f64, f64)>> = Vec::new();
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
            per_user.push(swarm(&active.url, count).await?);
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
            &measured.iter().map(|user| Some(user[index].0)).collect::<Vec<_>>(),
            base,
            Metric::Rate("req/s"),
            Absent::Failed,
        ));
        rows.push(row(
            &format!("{label}: p95 latency"),
            &measured.iter().map(|user| Some(user[index].1)).collect::<Vec<_>>(),
            base,
            Metric::Seconds,
            Absent::Failed,
        ));
    }
    rows.extend(cost_rows(servers, &costs));
    publish(
        "load",
        table("simple-page requests against a warm cache", servers, base, rows),
    )
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

/// Drive `users` concurrent resolver-style clients for 20 seconds; returns requests/s and p95.
async fn swarm(index_url: &str, users: usize) -> anyhow::Result<(f64, f64)> {
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
    let p95 = latencies[(latencies.len() * 95 / 100).min(latencies.len() - 1)];
    Ok((rps, p95))
}
