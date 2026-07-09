//! The request workload: a swarm of resolvers fetching project pages against each warm server, over
//! `rounds` restarts.

use anyhow::bail;
use hdrhistogram::Histogram;
use tokio::time::{Duration, Instant as TokioInstant, sleep_until};

use super::super::packages::TOP_PACKAGES;
use super::Rounds;
use crate::report::{Absent, Metric, baseline, cost_rows_per_request, publish, row, summarize, table};
use crate::servers::Server;
use crate::usage::{Cost, Usage};

/// The request workload: a swarm of resolvers fetching project pages against each warm server, over
/// `rounds` restarts.
///
/// # Errors
/// Returns an error when a server cannot start or its pages cannot be warmed.
pub async fn load(servers: &[Server], users: &[usize], rounds: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut rps: Vec<Vec<Vec<f64>>> = servers
        .iter()
        .map(|_| users.iter().map(|_| Vec::new()).collect())
        .collect();
    let mut p95: Vec<Vec<Vec<f64>>> = servers
        .iter()
        .map(|_| users.iter().map(|_| Vec::new()).collect())
        .collect();
    let mut costs: Vec<Option<Vec<Cost>>> = Vec::new();
    // One request total per round, aligned with that round's cost, so CPU can be priced per request.
    let mut served: Vec<Option<Vec<u64>>> = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        let mut collected = Rounds::new();
        let mut round_requests: Vec<u64> = Vec::new();
        for attempt in 1..=rounds {
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            let usage = Usage::watch(active.pid());
            println!("[load] {} round {attempt}/{rounds}", server.name);
            match load_round(&active.url, users, http).await {
                Ok(outcomes) => {
                    let mut total = 0;
                    for (slot, outcome) in outcomes.into_iter().enumerate() {
                        rps[index][slot].push(outcome.requests_per_second);
                        p95[index][slot].push(outcome.p95_seconds);
                        total += outcome.requests;
                    }
                    round_requests.push(total);
                }
                Err(error) => {
                    println!("[load] {} round {attempt}: failed ({error:#})", server.name);
                    round_requests.push(0);
                }
            }
            collected.record_cost(usage);
        }
        let ran = collected.costs();
        served.push(ran.as_ref().map(|_| round_requests));
        costs.push(ran);
    }
    let base = baseline(servers);
    let mut rows = Vec::new();
    for (slot, &count) in users.iter().enumerate() {
        let label = if count == 1 {
            "1 user".to_owned()
        } else {
            format!("{count} users")
        };
        let rps_slot: Vec<Vec<f64>> = rps.iter().map(|server| server[slot].clone()).collect();
        let p95_slot: Vec<Vec<f64>> = p95.iter().map(|server| server[slot].clone()).collect();
        rows.push(row(
            &format!("{label}: requests/s"),
            &summarize(&rps_slot),
            base,
            Metric::Rate("req/s"),
            Absent::Failed,
        ));
        rows.push(row(
            &format!("{label}: p95 latency"),
            &summarize(&p95_slot),
            base,
            Metric::Seconds,
            Absent::Failed,
        ));
    }
    rows.extend(cost_rows_per_request(servers, &costs, &served));
    publish(
        "load",
        table(
            "PEP 691 JSON simple-page requests against a warm cache: peak rate, and p95 latency at 70% of it",
            servers,
            base,
            rows,
        ),
    )
}

/// One round of the load workload against a warmed server: the swarm result per user count. Kept
/// fallible so a server that fails under load (a flaky competitor returning a 5xx) becomes an error
/// cell for that server rather than aborting the whole run.
async fn load_round(index_url: &str, users: &[usize], http: &reqwest::Client) -> anyhow::Result<Vec<SwarmResult>> {
    warm_pages(index_url, http).await?;
    let mut outcomes = Vec::with_capacity(users.len());
    for &count in users {
        outcomes.push(swarm(index_url, count).await?);
    }
    Ok(outcomes)
}

async fn warm_pages(index_url: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    for package in &TOP_PACKAGES[..10] {
        http.get(format!("{index_url}{package}/"))
            .header("Accept", super::SIMPLE_ACCEPT)
            .send()
            .await?
            .error_for_status()?;
    }
    Ok(())
}

/// One load result: the peak request rate the server sustains, and a coordinated-omission-corrected
/// p95 latency measured at a sustainable fraction of that rate.
struct SwarmResult {
    requests_per_second: f64,
    p95_seconds: f64,
    /// Every request the server answered in this swarm, so its CPU can be priced per unit of work.
    requests: u64,
}

/// How long the closed-loop capacity probe and the open-loop latency window each run.
const CAPACITY_WINDOW: Duration = Duration::from_secs(5);
const LATENCY_WINDOW: Duration = Duration::from_secs(15);

/// The latency probe drives at this fraction of the measured peak. Below capacity the fixed send
/// schedule keeps pace, so the tail reflects real per-request latency; at or above capacity the
/// backlog would grow without bound and every latency would swell toward the window length.
const LOAD_FRACTION: f64 = 0.7;

/// Measure `index_url` under `users` concurrent clients: the peak request rate it sustains, and the
/// p95 latency at [`LOAD_FRACTION`] of that rate.
///
/// The rate is a closed-loop burst (every client refetches the moment its response lands, so the
/// server stays saturated and the count over the window is its ceiling). The latency is a second,
/// open-loop pass paced on a fixed schedule at a sustainable rate: each request is timed from its
/// intended send time, so a stall is charged the full wait it caused. A closed-loop latency would stop
/// issuing exactly the requests a stall delays and miss the tail (coordinated omission, which
/// understates p99 by orders of magnitude); driving open-loop at or above capacity would instead
/// inflate every latency to the window length, which is why the schedule sits below the ceiling.
async fn swarm(index_url: &str, users: usize) -> anyhow::Result<SwarmResult> {
    let (requests_per_second, burst) = measure_capacity(index_url, users).await?;
    if requests_per_second == 0.0 {
        bail!("the swarm completed no requests");
    }
    let (p95_seconds, paced) = measure_tail(index_url, users, requests_per_second * LOAD_FRACTION).await?;
    Ok(SwarmResult {
        requests_per_second,
        p95_seconds,
        requests: burst + paced,
    })
}

/// Closed-loop peak: `users` clients each refetch as fast as responses return for [`CAPACITY_WINDOW`];
/// the completed count over the window is the sustained request rate.
async fn measure_capacity(index_url: &str, users: usize) -> anyhow::Result<(f64, u64)> {
    let mut tasks = Vec::new();
    for user in 0..users {
        let index_url = index_url.to_owned();
        tasks.push(tokio::spawn(async move {
            let client = reqwest::Client::builder().build().expect("client builds");
            let deadline = TokioInstant::now() + CAPACITY_WINDOW;
            let mut completed = 0u64;
            let mut page = user;
            while TokioInstant::now() < deadline {
                if fetch_page(&client, &format!("{index_url}{}/", TOP_PACKAGES[page % 10]))
                    .await
                    .is_ok()
                {
                    completed += 1;
                }
                page += 1;
            }
            completed
        }));
    }
    let mut completed = 0u64;
    for task in tasks {
        completed += task.await.expect("capacity client never panics");
    }
    #[expect(clippy::cast_precision_loss, reason = "request counts fit f64 exactly here")]
    Ok((completed as f64 / CAPACITY_WINDOW.as_secs_f64(), completed))
}

/// Open-loop tail: `users` clients share `target_rate` requests per second, each firing on a fixed
/// schedule regardless of when responses return, over [`LATENCY_WINDOW`]. Latency is timed from the
/// intended send time so a stall is charged its full delay. Returns the p95 in seconds and the
/// number of requests it recorded.
async fn measure_tail(index_url: &str, users: usize, target_rate: f64) -> anyhow::Result<(f64, u64)> {
    #[expect(clippy::cast_precision_loss, reason = "user counts fit f64 exactly here")]
    let interval = Duration::from_secs_f64(users as f64 / target_rate);
    let mut tasks = Vec::new();
    for user in 0..users {
        let index_url = index_url.to_owned();
        tasks.push(tokio::spawn(
            async move { tail_client(&index_url, user, interval).await },
        ));
    }
    let mut merged: Histogram<u64> = Histogram::new(3).expect("histogram bounds are valid");
    for task in tasks {
        merged
            .add(task.await.expect("tail client never panics"))
            .expect("histograms share bounds");
    }
    if merged.is_empty() {
        bail!("the latency probe recorded no requests");
    }
    #[expect(clippy::cast_precision_loss, reason = "microsecond latencies fit f64 exactly here")]
    Ok((merged.value_at_quantile(0.95) as f64 / 1e6, merged.len()))
}

/// One open-loop client: fetch on the `interval` schedule for [`LATENCY_WINDOW`], recording each
/// latency from its intended send time.
async fn tail_client(index_url: &str, user: usize, interval: Duration) -> Histogram<u64> {
    let client = reqwest::Client::builder().build().expect("client builds");
    let mut histogram: Histogram<u64> = Histogram::new(3).expect("histogram bounds are valid");
    let start = TokioInstant::now();
    let deadline = start + LATENCY_WINDOW;
    let mut intended = start;
    let mut page = user;
    while TokioInstant::now() < deadline {
        intended += interval;
        sleep_until(intended).await;
        if fetch_page(&client, &format!("{index_url}{}/", TOP_PACKAGES[page % 10]))
            .await
            .is_ok()
        {
            let latency = u64::try_from(intended.elapsed().as_micros()).unwrap_or(u64::MAX);
            histogram.record(latency).expect("latency is within histogram bounds");
        }
        page += 1;
    }
    histogram
}

async fn fetch_page(client: &reqwest::Client, target: &str) -> anyhow::Result<()> {
    client
        .get(target)
        .header("Accept", super::SIMPLE_ACCEPT)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    Ok(())
}
