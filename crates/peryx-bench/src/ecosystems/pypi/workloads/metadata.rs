//! The metadata workload: resolve a batch of PEP 658 siblings, cold then hot, over `rounds` restarts.

use std::time::Instant;

use anyhow::{Context as _, bail};

use super::super::packages::METADATA_PROJECT;
use super::{Rounds, median_or_dash_rate};
use crate::report::{Absent, Metric, baseline, cost_rows, network_row, publish, row, summarize, table};
use crate::servers::Server;
use crate::usage::{Cost, Usage};

/// The metadata workload: resolve a batch of PEP 658 siblings, cold then hot, over `rounds` restarts.
///
/// # Errors
/// Returns an error when a server cannot start; a server failing the metadata requests is a table
/// cell.
pub async fn metadata(servers: &[Server], rounds: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut cold: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut hot: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut rate: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut costs: Vec<Option<Vec<Cost>>> = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        let mut collected = Rounds::new();
        for attempt in 1..=rounds {
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            let usage = Usage::watch(active.pid());
            match metadata_round(&active.url, http).await {
                Ok((cold_seconds, hot_seconds, docs)) => {
                    cold[index].push(cold_seconds);
                    hot[index].push(hot_seconds);
                    rate[index].push(docs);
                }
                Err(error) => println!("[metadata] {} round {attempt}: failed ({error:#})", server.name),
            }
            collected.record_cost(usage);
        }
        println!(
            "[metadata] {}: hot {} docs/s",
            server.name,
            median_or_dash_rate(&rate[index])
        );
        costs.push(collected.costs());
    }
    let base = baseline(servers);
    let mut rows = vec![
        network_row(
            "cold metadata siblings",
            &summarize(&cold),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "hot metadata siblings",
            &summarize(&hot),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "hot metadata throughput",
            &summarize(&rate),
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

/// One round of the metadata workload: a cold batch (which caches the siblings), then a hot one.
async fn metadata_round(index_url: &str, http: &reqwest::Client) -> anyhow::Result<(f64, f64, f64)> {
    let urls = metadata_urls(index_url, http).await?;
    let cold = timed_metadata_batch(&urls, http).await?;
    let hot = timed_metadata_batch(&urls, http).await?;
    #[expect(clippy::cast_precision_loss, reason = "metadata batch lengths fit f64 exactly")]
    Ok((cold, hot, urls.len() as f64 / hot))
}

async fn metadata_urls(index_url: &str, http: &reqwest::Client) -> anyhow::Result<Vec<String>> {
    const LIMIT: usize = 16;
    let response = http
        .get(format!("{index_url}{METADATA_PROJECT}/"))
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
