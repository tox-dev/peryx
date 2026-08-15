use std::time::Instant;

use anyhow::Context as _;

use super::super::packages::STRESS_PROJECT;
use super::{Rounds, median_or_dash_rate};
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::report::{Absent, Metric, baseline, cost_rows, network_row, row, summarize, table};
use peryx_bench_core::servers::{Active, Server};
use peryx_bench_core::usage::{Cost, Usage};

#[cfg(target_os = "macos")]
const STRESS_TAGS: &[&str] = &["macosx", "arm64"];
#[cfg(not(target_os = "macos"))]
const STRESS_TAGS: &[&str] = &["manylinux", "x86_64"];

/// The file-transfer workload: one large wheel, cold under contention and hot at full speed.
///
/// The cold row sends four clients after the same uncached wheel at once, which is what a CI fleet
/// does to a cache the moment a new release lands: it measures whether the server fans one upstream
/// transfer out to every waiter or serializes them. The hot rows measure how fast a cached wheel
/// leaves the server, alone and under eight parallel readers. Each round restarts the server, so cold
/// is cold every time.
///
/// # Errors
/// Returns an error when a server cannot start; a server failing the transfers is a table cell.
pub async fn throughput(
    context: &BenchmarkContext,
    servers: &[Server],
    rounds: usize,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    throughput_from(context, servers, rounds, http, "https://pypi.org/simple/").await
}

async fn throughput_from(
    context: &BenchmarkContext,
    servers: &[Server],
    rounds: usize,
    http: &reqwest::Client,
    source_index: &str,
) -> anyhow::Result<()> {
    if servers.is_empty() || rounds == 0 {
        let empty = vec![Vec::new(); servers.len()];
        return publish_throughput(context, servers, &empty, &empty, &empty, &[]);
    }
    let filename = stress_wheel_filename(source_index, http).await?;
    println!("[throughput] measuring with {filename}");
    let mut cold4: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut hot1: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut hot8: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut costs: Vec<Option<Vec<Cost>>> = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        let mut collected = Rounds::new();
        for attempt in 1..=rounds {
            let scratch = tempfile::tempdir_in(context.scratch())?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(context, &state, http).await?;
            let usage = Usage::watch(active.pid())?;
            match transfer_round(&active, &filename, http).await {
                Ok((cold, single, eight)) => {
                    cold4[index].push(cold);
                    hot1[index].push(single);
                    hot8[index].push(eight);
                }
                Err(error) => println!("[throughput] {} round {attempt}: failed ({error:#})", server.name),
            }
            collected.record_cost(usage)?;
        }
        println!(
            "[throughput] {}: hot {} MB/s, hot-8 {} MB/s",
            server.name,
            median_or_dash_rate(&hot1[index]),
            median_or_dash_rate(&hot8[index]),
        );
        costs.push(collected.costs());
    }
    publish_throughput(context, servers, &cold4, &hot1, &hot8, &costs)
}

fn publish_throughput(
    context: &BenchmarkContext,
    servers: &[Server],
    cold4: &[Vec<f64>],
    hot1: &[Vec<f64>],
    hot8: &[Vec<f64>],
    costs: &[Option<Vec<Cost>>],
) -> anyhow::Result<()> {
    let base = baseline(servers);
    let mut rows = vec![
        network_row(
            "cold cache: 4 clients, one wheel",
            &summarize(cold4),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "hot cache: single download",
            &summarize(hot1),
            base,
            Metric::Rate("MB/s"),
            Absent::Failed,
        ),
        row(
            "hot cache: 8 parallel downloads",
            &summarize(hot8),
            base,
            Metric::Rate("MB/s"),
            Absent::Failed,
        ),
    ];
    rows.extend(cost_rows(servers, costs));
    context.publish(
        "throughput",
        table(
            &format!("moving one large wheel ({STRESS_PROJECT}): cold under contention, hot at speed"),
            servers,
            base,
            rows,
        ),
    )
}

/// One round of the transfer workload: four cold clients (which also warm the cache), then a single
/// and an eight-way hot download of the now-cached wheel.
async fn transfer_round(active: &Active, filename: &str, http: &reqwest::Client) -> anyhow::Result<(f64, f64, f64)> {
    let url = wheel_url(&active.url, STRESS_PROJECT, filename, http).await?;
    let cold4 = parallel_downloads(&url, 4, http).await?;
    let (single_seconds, size) = timed_download(&url, http).await?;
    let hot8_wall = parallel_downloads(&url, 8, http).await?;
    #[expect(clippy::cast_precision_loss, reason = "wheel sizes fit f64 to the byte")]
    Ok((
        cold4,
        size as f64 / single_seconds / 1e6,
        8.0 * size as f64 / hot8_wall / 1e6,
    ))
}

/// The concrete wheel every server moves, resolved once from `PyPI` so all parties match.
async fn stress_wheel_filename(source_index: &str, http: &reqwest::Client) -> anyhow::Result<String> {
    let body = http
        .get(format!("{source_index}{STRESS_PROJECT}/"))
        .header("Accept", "application/vnd.pypi.simple.v1+json")
        .send()
        .await?
        .text()
        .await?;
    let page: serde_json::Value = serde_json::from_str(&body)?;
    page["files"]
        .as_array()
        .context("simple JSON has no files")?
        .iter()
        .filter_map(|file| file["filename"].as_str())
        .rfind(|name| STRESS_TAGS.iter().all(|tag| name.contains(tag)))
        .map(str::to_owned)
        .context("no wheel matches this platform")
}

/// Resolve `filename`'s download URL through a server's simple page, JSON or HTML alike.
async fn wheel_url(index_url: &str, project: &str, filename: &str, http: &reqwest::Client) -> anyhow::Result<String> {
    let response = http
        .get(format!("{index_url}{project}/"))
        .header("Accept", super::SIMPLE_ACCEPT)
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

/// The first `href="…"` on the page whose target mentions `filename`; no HTML parser needed for the
/// anchor-list pages every simple index serves.
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
    let mut outcome: anyhow::Result<()> = Ok(());
    for download in downloads {
        let current = download
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result.map(|_| ()));
        if outcome.is_ok() {
            outcome = current;
        }
    }
    outcome?;
    Ok(start.elapsed().as_secs_f64())
}

#[cfg(test)]
#[path = "../../../tests/unit/bench/workloads/throughput.rs"]
mod tests;
