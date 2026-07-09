//! The endpoints workload: what one warm request to each endpoint peryx serves costs.
//!
//! The other workloads drive a real client and measure what a user feels, so they only ever touch the
//! three endpoints an installer touches. That leaves most of the surface unmeasured, and an
//! unmeasured endpoint is where a regression hides: the simple index root, the two PEP 503 HTML
//! representations, the legacy JSON API, the PEP 658 sibling, and peryx's archive introspection.
//!
//! Each row is one endpoint, timed warm: the server has already answered it once, so the row prices
//! serving rather than filling a cache.
//!
//! This one table is peryx against itself, not against the field. Unlike OCI, whose `/v2/` paths the
//! distribution spec fixes, a `PyPI` server is free to shape its own urls and to decide what its index
//! root even contains: pypi.org answers `/pypi/{project}/json` where peryx answers
//! `{index}/{project}/json`, devpi addresses files by an internal path, and a proxy's index root lists
//! what it has cached while pypi.org's lists every project that exists. Rows across those servers
//! would compare different work and read as a ranking. The cross-server comparisons live in the
//! workloads above, which drive one client against all of them.

use std::time::Instant;

use anyhow::{Context as _, bail};

use super::super::packages::METADATA_PROJECT;
use super::{Rounds, SIMPLE_ACCEPT, SIMPLE_ACCEPT_HTML};
use crate::report::{Absent, Metric, baseline, cost_rows, publish, row, summarize, table};
use crate::servers::Server;
use crate::stats::Summary;
use crate::usage::{Cost, Usage};

/// Requests per endpoint per round. The median of these is the round's sample, so a single scheduling
/// hiccup cannot decide a cell.
const PROBES: usize = 25;

/// The endpoints, in the order they appear as table rows.
///
/// Held as a list rather than a match so the table's shape and the request list cannot drift apart.
const ENDPOINTS: [&str; 7] = [
    "simple index (JSON)",
    "simple index (HTML)",
    "project detail (JSON)",
    "project detail (HTML)",
    "legacy project JSON",
    "PEP 658 metadata sibling",
    "archive inspect listing",
];

/// The endpoints workload: one warm request to every endpoint peryx serves.
///
/// # Errors
/// Returns an error when peryx cannot start; an endpoint that fails is an empty cell.
pub async fn endpoints(servers: &[Server], rounds: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let Some(position) = servers.iter().position(|server| server.name == "peryx") else {
        return Ok(());
    };
    let servers = &servers[position..=position];
    let mut samples: Vec<Vec<Vec<f64>>> = ENDPOINTS
        .iter()
        .map(|_| servers.iter().map(|_| Vec::new()).collect())
        .collect();
    let mut costs: Vec<Option<Vec<Cost>>> = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        let mut collected = Rounds::new();
        for attempt in 1..=rounds {
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            let usage = Usage::watch(active.pid());
            match endpoint_round(&active.url, http).await {
                Ok(seconds) => {
                    for (endpoint, sample) in seconds.iter().enumerate() {
                        if let Some(sample) = *sample {
                            samples[endpoint][index].push(sample);
                        }
                    }
                }
                Err(error) => println!("[endpoints] {} round {attempt}: failed ({error:#})", server.name),
            }
            collected.record_cost(usage);
        }
        println!("[endpoints] {}: {}", server.name, served(&samples, index));
        costs.push(collected.costs());
    }
    let base = baseline(servers);
    let mut rows: Vec<_> = ENDPOINTS
        .iter()
        .enumerate()
        .map(|(endpoint, name)| {
            row(
                name,
                &summarize(&samples[endpoint]),
                base,
                Metric::Seconds,
                Absent::Failed,
            )
        })
        .collect();
    rows.extend(cost_rows(servers, &costs));
    publish(
        "endpoints",
        table(
            "peryx only: one warm request to each endpoint it serves. A PyPI server shapes its own urls, \
             so these rows do not compare across the field",
            servers,
            base,
            rows,
        ),
    )
}

/// How many of the endpoints this server answered, for the live log.
fn served(samples: &[Vec<Vec<f64>>], index: usize) -> String {
    let answered = samples.iter().filter(|endpoint| !endpoint[index].is_empty()).count();
    format!("{answered}/{} endpoints", ENDPOINTS.len())
}

/// Time one warm request to each endpoint, `None` where the server does not serve it.
///
/// The index URL a server exposes ends in `simple/`, so the sibling APIs sit one level above it.
async fn endpoint_round(index_url: &str, http: &reqwest::Client) -> anyhow::Result<Vec<Option<f64>>> {
    let root = index_url
        .strip_suffix("simple/")
        .context("the server's index url does not end in simple/")?;
    let project = format!("{index_url}{METADATA_PROJECT}/");
    // Warm the project page before anything is timed: every row below prices serving, and the first
    // request through a proxy fetches the page from the real upstream.
    let page = fetch(http, &project, SIMPLE_ACCEPT).await?;
    let Some(relative) = first_file_url(&page) else {
        bail!("{METADATA_PROJECT}'s page carried no file url");
    };
    // A simple page addresses its files relative to itself, so the link only becomes a request once
    // it is joined back onto the page that carried it.
    let file = url::Url::parse(&project)
        .context("the project page url does not parse")?
        .join(&relative)
        .with_context(|| format!("{relative} is not a url relative to {project}"))?
        .to_string();

    Ok(vec![
        probe(http, index_url, SIMPLE_ACCEPT).await,
        probe(http, index_url, SIMPLE_ACCEPT_HTML).await,
        probe(http, &project, SIMPLE_ACCEPT).await,
        probe(http, &project, SIMPLE_ACCEPT_HTML).await,
        probe(http, &format!("{root}{METADATA_PROJECT}/json"), SIMPLE_ACCEPT).await,
        probe(http, &format!("{file}.metadata"), "*/*").await,
        probe(http, &inspect_url(root, &file), SIMPLE_ACCEPT).await,
    ])
}

/// peryx's archive listing lives beside the file it introspects, under `inspect/` in place of `files/`.
fn inspect_url(root: &str, file: &str) -> String {
    file.rfind("/files/").map_or_else(
        || format!("{root}inspect/missing"),
        |cut| format!("{}/inspect/{}", &file[..cut], &file[cut + "/files/".len()..]),
    )
}

/// The median warm latency of `PROBES` requests, or `None` when the server does not serve `url`.
///
/// One untimed request first, so the row prices a warm response rather than whatever filling the
/// cache costs; a non-success answer means the endpoint is absent, which is a cell, not an error.
async fn probe(http: &reqwest::Client, url: &str, accept: &str) -> Option<f64> {
    drain(http, url, accept).await.ok()?;
    let mut latencies = Vec::with_capacity(PROBES);
    for _ in 0..PROBES {
        let start = Instant::now();
        drain(http, url, accept).await.ok()?;
        latencies.push(start.elapsed().as_secs_f64());
    }
    Summary::of(&latencies).map(|summary| summary.median)
}

/// Read `url`'s body and drop it, so the timing covers the whole response and nothing else.
///
/// Deliberately not `text()`: decoding a half-megabyte page into a `String` validates its UTF-8 on
/// every probe, which put the client's work inside the server's row and made a 130 µs page read 1.3 ms.
async fn drain(http: &reqwest::Client, url: &str, accept: &str) -> anyhow::Result<()> {
    let mut response = http
        .get(url)
        .header("Accept", accept)
        .send()
        .await?
        .error_for_status()?;
    while response.chunk().await?.is_some() {}
    Ok(())
}

/// Fetch `url` as text, for the one untimed request whose body has to be parsed.
async fn fetch(http: &reqwest::Client, url: &str, accept: &str) -> anyhow::Result<String> {
    let response = http
        .get(url)
        .header("Accept", accept)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.text().await?)
}

/// The first artifact url on a simple page, in either representation.
///
/// Each server names its files differently (peryx addresses them by digest, devpi by an internal
/// path), so the url has to come from the page the server itself rendered.
fn first_file_url(page: &str) -> Option<String> {
    if let Ok(detail) = serde_json::from_str::<serde_json::Value>(page) {
        return detail
            .get("files")?
            .as_array()?
            .iter()
            .find_map(|file| file.get("url")?.as_str().map(str::to_owned));
    }
    let start = page.find("href=\"")? + "href=\"".len();
    let rest = &page[start..];
    let end = rest.find('"')?;
    Some(rest[..end].split('#').next()?.to_owned())
}
