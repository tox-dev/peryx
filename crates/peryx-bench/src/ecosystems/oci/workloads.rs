//! The three OCI workloads: image pulls, blob throughput, and a parallel pull fleet.
//!
//! Every workload drives `crane` for the transfers, so one client handles the bearer-token dance
//! against Docker Hub and plain pulls against the local proxies alike, and no registry sees a
//! client-side layer cache between measurements.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context as _, bail};
use tokio::process::Command;

use super::images::{FLEET_IMAGE, PULL_IMAGES, STRESS_IMAGE};
use super::servers::{DOCKERHUB, client_reference, insecure, table_name, upstream_for};
use crate::report::{Absent, Metric, baseline, network_row, publish, row, summarize, table};
use crate::servers::{Active, Server};
use crate::stats::Summary;

/// The host architecture in the terms an image index's platform entries use.
fn docker_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// The pull workload: fetch every image through each registry, cold then warm.
///
/// Cold starts the registry with empty state, so each layer is a miss it must fetch from Docker Hub;
/// warm reruns against the now-full cache. crane writes each image to a throwaway tarball, so no
/// client-side layer cache carries between the two passes.
///
/// # Errors
/// Returns an error when a registry cannot start; a registry failing the pulls is a table cell.
pub async fn pulls(servers: &[Server], rounds: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut cold: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut warm: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    for (index, server) in servers.iter().enumerate() {
        for attempt in 1..=rounds {
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            match pull_round(&active, scratch.path()).await {
                Ok((cold_seconds, warm_seconds)) => {
                    cold[index].push(cold_seconds);
                    warm[index].push(warm_seconds);
                }
                Err(error) => println!("[pull] {} round {attempt}: failed ({error:#})", server.name),
            }
        }
        println!(
            "[pull] {}: cold {} warm {}",
            server.name,
            median_text(&cold[index], "s"),
            median_text(&warm[index], "s"),
        );
    }
    let base = baseline(servers);
    let rows = vec![
        network_row("cold cache", &summarize(&cold), base, Metric::Seconds, Absent::Failed),
        row("warm cache", &summarize(&warm), base, Metric::Seconds, Absent::Failed),
    ];
    publish(
        &table_name("pull"),
        table(
            &format!("pull {} images through each registry", PULL_IMAGES.len()),
            servers,
            base,
            rows,
        ),
    )
}

async fn pull_round(active: &Active, scratch: &Path) -> anyhow::Result<(f64, f64)> {
    let cold = pull_all(&active.url, scratch).await?;
    let warm = pull_all(&active.url, scratch).await?;
    Ok((cold, warm))
}

/// The median of `samples` with a unit suffix, or a dash when every round failed; for live logging.
fn median_text(samples: &[f64], suffix: &str) -> String {
    Summary::of(samples).map_or_else(|| "-".to_owned(), |summary| format!("{:.1}{suffix}", summary.median))
}

/// Pull every image once through `base`, to throwaway tarballs; returns wall seconds.
async fn pull_all(base: &str, scratch: &Path) -> anyhow::Result<f64> {
    let start = Instant::now();
    for (index, image) in PULL_IMAGES.iter().enumerate() {
        let dest = scratch.join(format!("image-{index}.tar"));
        crane_pull(base, image, &dest).await?;
        let _ = std::fs::remove_file(&dest);
    }
    Ok(start.elapsed().as_secs_f64())
}

/// A freshly restarted proxy (distribution, zot) can answer `/v2/`, and so pass the readiness probe,
/// before its upstream connection is ready, so the first pull of an uncached image races and fails.
/// peryx serves on the first try and never retries; the retry only spares a competitor from an
/// error cell it would otherwise earn for a startup race rather than a real failure.
const PULL_ATTEMPTS: usize = 3;

/// One `crane pull` of `image` through `base` into `dest`, retried past a proxy's startup race.
async fn crane_pull(base: &str, image: &str, dest: &Path) -> anyhow::Result<()> {
    let mut last = None;
    for attempt in 1..=PULL_ATTEMPTS {
        match crane_pull_once(base, image, dest).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if attempt < PULL_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                last = Some(error);
            }
        }
    }
    Err(last.expect("the loop runs at least once"))
}

async fn crane_pull_once(base: &str, image: &str, dest: &Path) -> anyhow::Result<()> {
    let mut command = Command::new("crane");
    command.arg("pull");
    if insecure(base) {
        command.arg("--insecure");
    }
    command.arg(client_reference(base, image)).arg(dest);
    let output = command.output().await.context("crane did not start")?;
    if !output.status.success() {
        bail!(
            "crane pull {image} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// The throughput workload: stream one large cached layer, alone and under eight parallel readers.
///
/// Each registry is warmed with the image first, so every one holds the layer however it fills its
/// cache, and the rows then compare how fast that cached layer leaves it: the transfer rate a warm
/// cache serves at, which is what a client on a real network feels once the first pull has landed.
///
/// # Errors
/// Returns an error when a registry cannot start or the stress layer cannot be resolved.
pub async fn throughput(servers: &[Server], rounds: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let (digest, size) = largest_layer(&upstream_for(DOCKERHUB), STRESS_IMAGE).await?;
    #[expect(clippy::cast_precision_loss, reason = "layer sizes fit f64 to the byte")]
    let megabytes = size as f64 / 1e6;
    println!("[throughput] streaming {STRESS_IMAGE}'s {megabytes:.0} MB layer {digest}");
    let mut hot1: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut hot8: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    for (index, server) in servers.iter().enumerate() {
        for attempt in 1..=rounds {
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            match blob_round(&active.url, &digest, size).await {
                Ok((single, eight)) => {
                    hot1[index].push(single);
                    hot8[index].push(eight);
                }
                Err(error) => println!("[throughput] {} round {attempt}: failed ({error:#})", server.name),
            }
        }
        println!(
            "[throughput] {}: hot {} MB/s, hot-8 {} MB/s",
            server.name,
            median_text(&hot1[index], ""),
            median_text(&hot8[index], ""),
        );
    }
    let base = baseline(servers);
    let rows = vec![
        row(
            "hot cache: single stream",
            &summarize(&hot1),
            base,
            Metric::Rate("MB/s"),
            Absent::Failed,
        ),
        row(
            "hot cache: 8 parallel streams",
            &summarize(&hot8),
            base,
            Metric::Rate("MB/s"),
            Absent::Failed,
        ),
    ];
    publish(
        &table_name("image-throughput"),
        table(
            &format!("streaming one large cached layer ({STRESS_IMAGE}), alone and eight-way"),
            servers,
            base,
            rows,
        ),
    )
}

/// One round of the throughput workload: warm the layer with a full pull, then one single and one
/// eight-way stream of the cached layer.
async fn blob_round(base: &str, digest: &str, size: u64) -> anyhow::Result<(f64, f64)> {
    let repo = repository(STRESS_IMAGE);
    // Warm the layer by pulling the whole image first. A pull-through proxy fetches the layer on
    // demand and a sync-based registry mirrors it from the manifest, so this is the one request all
    // registries answer alike; measuring a bare blob against a cold sync-based registry would price
    // a request it never serves. The hot rows below then compare cached-layer transfer like for like.
    let scratch = tempfile::tempdir()?;
    crane_pull(base, STRESS_IMAGE, &scratch.path().join("warm.tar")).await?;
    let single = crane_blob(base, &repo, digest).await?;
    let hot8 = parallel_blobs(base, &repo, digest, 8).await?;
    #[expect(clippy::cast_precision_loss, reason = "layer sizes fit f64 to the byte")]
    let (size, clients) = (size as f64, 8.0);
    Ok((size / single / 1e6, clients * size / hot8 / 1e6))
}

/// One `crane blob` of `repo@digest` through `base`, streamed to nowhere; returns wall seconds.
async fn crane_blob(base: &str, repo: &str, digest: &str) -> anyhow::Result<f64> {
    let reference = format!("{}@{digest}", client_reference(base, repo));
    let mut command = Command::new("crane");
    command.arg("blob");
    if insecure(base) {
        command.arg("--insecure");
    }
    command.arg(&reference).stdout(Stdio::null()).stderr(Stdio::piped());
    let start = Instant::now();
    let output = command.output().await.context("crane did not start")?;
    if !output.status.success() {
        bail!(
            "crane blob {reference} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(start.elapsed().as_secs_f64())
}

/// `clients` simultaneous streams of the same layer; returns wall seconds until all finish.
async fn parallel_blobs(base: &str, repo: &str, digest: &str, clients: usize) -> anyhow::Result<f64> {
    let start = Instant::now();
    let streams: Vec<_> = (0..clients)
        .map(|_| {
            let (base, repo, digest) = (base.to_owned(), repo.to_owned(), digest.to_owned());
            tokio::spawn(async move { crane_blob(&base, &repo, &digest).await })
        })
        .collect();
    for stream in streams {
        stream.await.expect("blob task never panics")?;
    }
    Ok(start.elapsed().as_secs_f64())
}

/// Resolve the largest layer of `image` (its digest and size) through `base`, picking the host
/// platform out of a multi-arch index.
async fn largest_layer(base: &str, image: &str) -> anyhow::Result<(String, u64)> {
    let reference = client_reference(base, image);
    let insecure = insecure(base);
    let top = crane_manifest(&reference, insecure, None).await?;
    let manifest = if let Some(entries) = top["manifests"].as_array() {
        let digest = entries
            .iter()
            .find(|entry| entry["platform"]["architecture"] == docker_arch() && entry["platform"]["os"] == "linux")
            .and_then(|entry| entry["digest"].as_str())
            .with_context(|| format!("{image} has no linux/{} manifest", docker_arch()))?;
        crane_manifest(&reference, insecure, Some(digest)).await?
    } else {
        top
    };
    let layer = manifest["layers"]
        .as_array()
        .context("manifest has no layers")?
        .iter()
        .max_by_key(|layer| layer["size"].as_u64().unwrap_or(0))
        .context("manifest lists no layers")?;
    let digest = layer["digest"].as_str().context("layer has no digest")?.to_owned();
    let size = layer["size"].as_u64().context("layer has no size")?;
    Ok((digest, size))
}

/// `crane manifest` for a reference, optionally pinned to an index entry's digest, as parsed JSON.
async fn crane_manifest(reference: &str, insecure: bool, digest: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let target = digest.map_or_else(|| reference.to_owned(), |digest| format!("{reference}@{digest}"));
    let mut command = Command::new("crane");
    command.arg("manifest");
    if insecure {
        command.arg("--insecure");
    }
    command.arg(&target);
    let output = command.output().await.context("crane did not start")?;
    if !output.status.success() {
        bail!(
            "crane manifest {target} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).context("crane manifest returned invalid JSON")
}

/// The fleet workload: ten clients pull the same image at once, cold then warm.
///
/// Cold is the moment a CI runner pool reaches for a fresh image together: the registry either fans
/// one upstream pull out to every waiter or serializes them.
///
/// # Errors
/// Returns an error when a registry cannot start; a registry failing the fleet is a table cell.
pub async fn fleet(servers: &[Server], rounds: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut cold: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    let mut warm: Vec<Vec<f64>> = servers.iter().map(|_| Vec::new()).collect();
    for (index, server) in servers.iter().enumerate() {
        for attempt in 1..=rounds {
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
            match fleet_round(&active.url, scratch.path()).await {
                Ok((cold_seconds, warm_seconds)) => {
                    cold[index].push(cold_seconds);
                    warm[index].push(warm_seconds);
                }
                Err(error) => println!("[fleet] {} round {attempt}: failed ({error:#})", server.name),
            }
        }
        println!(
            "[fleet] {}: cold {} warm {}",
            server.name,
            median_text(&cold[index], "s"),
            median_text(&warm[index], "s"),
        );
    }
    let base = baseline(servers);
    let rows = vec![
        network_row(
            "cold cache: 10 parallel pulls",
            &summarize(&cold),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
        row(
            "warm cache: 10 parallel pulls",
            &summarize(&warm),
            base,
            Metric::Seconds,
            Absent::Failed,
        ),
    ];
    publish(
        &table_name("parallel-pull"),
        table(&format!("ten clients pull {FLEET_IMAGE} at once"), servers, base, rows),
    )
}

/// One round of the fleet workload: ten cold pulls, then ten warm ones against the same registry.
async fn fleet_round(base: &str, scratch: &Path) -> anyhow::Result<(f64, f64)> {
    let cold = fleet_pull(base, scratch, 10).await?;
    let warm = fleet_pull(base, scratch, 10).await?;
    Ok((cold, warm))
}

/// Pull `FLEET_IMAGE` into `workers` throwaway tarballs at once; returns wall seconds.
async fn fleet_pull(base: &str, scratch: &Path, workers: usize) -> anyhow::Result<f64> {
    let start = Instant::now();
    let pulls: Vec<_> = (0..workers)
        .map(|worker| {
            let (base, dest) = (base.to_owned(), scratch.join(format!("fleet-{worker}.tar")));
            tokio::spawn(async move {
                let result = crane_pull(&base, FLEET_IMAGE, &dest).await;
                let _ = std::fs::remove_file(&dest);
                result
            })
        })
        .collect();
    for pull in pulls {
        pull.await.expect("fleet worker never panics")?;
    }
    Ok(start.elapsed().as_secs_f64())
}

/// The repository of an image reference, its `:tag` dropped.
fn repository(image: &str) -> String {
    split_tag(image).0.to_owned()
}

/// Split an image reference into its repository and tag.
fn split_tag(image: &str) -> (&str, &str) {
    image.rsplit_once(':').unwrap_or((image, "latest"))
}
