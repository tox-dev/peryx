//! The three OCI workloads: image pulls, blob throughput, and a parallel pull fleet.
//!
//! Pulls drive `crane`, so one client handles the bearer-token dance against Docker Hub and plain
//! pulls against the local proxies alike, and no registry sees a client-side layer cache between
//! measurements. Blob throughput is read in process instead: a `crane` launch per stream costs more
//! than the transfer it wraps, and it lands unevenly across the single and eight-way rows.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context as _, bail};
use tokio::process::Command;

use super::images::{FLEET_IMAGE, PULL_IMAGES, STRESS_IMAGE};
use super::servers::{DOCKERHUB, client_reference, hub_credentials, insecure, table_name, upstream_for};
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
            match blob_round(http, &active.url, &digest, size).await {
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
async fn blob_round(http: &reqwest::Client, base: &str, digest: &str, size: u64) -> anyhow::Result<(f64, f64)> {
    let repo = repository(STRESS_IMAGE);
    // Warm the layer by pulling the whole image first. A pull-through proxy fetches the layer on
    // demand and a sync-based registry mirrors it from the manifest, so this is the one request all
    // registries answer alike; measuring a bare blob against a cold sync-based registry would price
    // a request it never serves. The hot rows below then compare cached-layer transfer like for like.
    let scratch = tempfile::tempdir()?;
    crane_pull(base, STRESS_IMAGE, &scratch.path().join("warm.tar")).await?;
    // Read it once more so the page cache holds it for both rows, and neither prices a disk read.
    stream_blob(http, base, &repo, digest, size).await?;
    let single = stream_blob(http, base, &repo, digest, size).await?;
    let hot8 = parallel_blobs(http, base, &repo, digest, size, 8).await?;
    #[expect(clippy::cast_precision_loss, reason = "layer sizes fit f64 to the byte")]
    let (size, clients) = (size as f64, 8.0);
    Ok((size / single / 1e6, clients * size / hot8 / 1e6))
}

/// The distribution-spec blob URL for `repo@digest` behind `base`, keeping whatever index prefix the
/// base carries: peryx serves `/v2/<index>/<repo>/blobs/…`, a bare registry `/v2/<repo>/blobs/…`.
fn blob_url(base: &str, repo: &str, digest: &str) -> anyhow::Result<String> {
    v2_url(base, &format!("{repo}/blobs/{digest}"))
}

/// A `/v2/` URL under `base`, keeping whatever index prefix the base carries.
fn v2_url(base: &str, rest: &str) -> anyhow::Result<String> {
    let url = url::Url::parse(base).context("registry base is a valid URL")?;
    let prefix = url.path().trim_matches('/');
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    Ok(format!("{}v2/{prefix}{rest}", origin(&url)?))
}

/// The version check is the one endpoint that is never index-scoped: the spec puts it at `/v2/`, and
/// peryx answers it there before it looks for an index at all.
fn version_url(base: &str) -> anyhow::Result<String> {
    let url = url::Url::parse(base).context("registry base is a valid URL")?;
    Ok(format!("{}v2/", origin(&url)?))
}

/// `scheme://host[:port]/` for `url`.
fn origin(url: &url::Url) -> anyhow::Result<String> {
    let host = url.host_str().context("registry base names a host")?;
    let authority = url
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
    Ok(format!("{}://{authority}/", url.scheme()))
}

/// Read `repo@digest` through `base` with this harness's own client, discarding the bytes; returns
/// wall seconds.
///
/// This used to shell out to `crane blob` per stream. A process launch plus crane's client-side
/// sha256 verify are a large fixed cost against a transfer that takes a fifth of a second, and the
/// cost lands unevenly: it is paid once against a single stream and amortized across eight parallel
/// ones, so the same distortion penalized the single-stream row and flattered the eight-way row.
/// Worse, it decided the ranking. Served to a plain HTTP client, peryx streams this layer faster than
/// zot; measured through `crane`, it appeared to lose by half. Reading the blob in process prices the
/// registry, exactly as the `PyPI` throughput workload already does.
/// Stream one blob and return the seconds it took, having checked every byte arrived.
///
/// Counting the body is not paranoia: a registry that answers `200` with a short body would divide
/// the layer's full size by a fraction of the transfer time and report a throughput it never reached.
async fn stream_blob(http: &reqwest::Client, base: &str, repo: &str, digest: &str, size: u64) -> anyhow::Result<f64> {
    let url = blob_url(base, repo, digest)?;
    let start = Instant::now();
    let mut response = http.get(&url).send().await.context("blob request did not send")?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let token = bearer_token(http, &response, repo).await?;
        response = http.get(&url).bearer_auth(token).send().await?;
    }
    if !response.status().is_success() {
        bail!("blob {url} answered {}", response.status());
    }
    let mut served = 0u64;
    while let Some(chunk) = response.chunk().await? {
        served += chunk.len() as u64;
    }
    let elapsed = start.elapsed().as_secs_f64();
    anyhow::ensure!(served == size, "blob {url} served {served} bytes, expected {size}");
    Ok(elapsed)
}

/// Exchange a registry's `401` challenge for a pull token. Docker Hub is the only party that asks.
async fn bearer_token(http: &reqwest::Client, challenge: &reqwest::Response, repo: &str) -> anyhow::Result<String> {
    let header = challenge
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .context("a 401 carries a WWW-Authenticate challenge")?;
    let field = |name: &str| {
        header
            .split([',', ' '])
            .find_map(|part| part.trim().strip_prefix(&format!("{name}=")))
            .map(|value| value.trim_matches('"').to_owned())
    };
    let realm = field("realm").context("the challenge names a bearer realm")?;
    let service = field("service").unwrap_or_default();
    let scope = format!("repository:{repo}:pull");
    let endpoint = url::Url::parse_with_params(&realm, [("service", service.as_str()), ("scope", scope.as_str())])
        .context("the bearer realm is a valid URL")?;
    let mut request = http.get(endpoint);
    if let Some((user, secret)) = hub_credentials() {
        request = request.basic_auth(user, Some(secret));
    }
    let body: serde_json::Value =
        serde_json::from_str(&request.send().await?.text().await?).context("the token endpoint returned JSON")?;
    body.get("token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("the token endpoint returned a token")
}

/// `clients` simultaneous streams of the same layer; returns wall seconds until all finish.
async fn parallel_blobs(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    digest: &str,
    size: u64,
    clients: usize,
) -> anyhow::Result<f64> {
    let start = Instant::now();
    let streams: Vec<_> = (0..clients)
        .map(|_| {
            let (http, base, repo, digest) = (http.clone(), base.to_owned(), repo.to_owned(), digest.to_owned());
            tokio::spawn(async move { stream_blob(&http, &base, &repo, &digest, size).await })
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

/// Requests per endpoint per round; the median of these is the round's sample.
const PROBES: usize = 25;

/// What a client that understands both image manifests and multi-arch indexes asks for.
///
/// The distribution spec makes the manifest representation a matter of `Accept`, and no other
/// workload sends these: `crane` negotiates inside its own process, so the header never appears in
/// anything this harness controls.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

/// The endpoints, in the order they appear as table rows.
const ENDPOINTS: [&str; 9] = [
    "version check",
    "manifest by tag",
    "manifest by tag (HEAD)",
    "manifest by digest",
    "blob (HEAD)",
    "config blob",
    "layer range (1 MiB)",
    "tag list",
    "tag list (paginated)",
];

/// The endpoints workload: one warm request to every endpoint the registry serves.
///
/// The pull and throughput workloads only ever exercise the version check, a manifest, and a blob,
/// because that is all `crane pull` needs. Everything a real client also does — the `HEAD` that
/// checks a layer before fetching it, a ranged resume, listing tags — is priced here.
///
/// # Errors
/// Returns an error when a registry cannot start; a registry not serving an endpoint is an empty cell.
pub async fn endpoints(servers: &[Server], rounds: usize, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut samples: Vec<Vec<Vec<f64>>> = ENDPOINTS
        .iter()
        .map(|_| servers.iter().map(|_| Vec::new()).collect())
        .collect();
    for (index, server) in servers.iter().enumerate() {
        for attempt in 1..=rounds {
            let scratch = tempfile::tempdir()?;
            let state = scratch.path().join("state");
            std::fs::create_dir(&state)?;
            let active = server.start(&state, http).await?;
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
        }
        let answered = samples.iter().filter(|endpoint| !endpoint[index].is_empty()).count();
        println!("[endpoints] {}: {answered}/{} endpoints", server.name, ENDPOINTS.len());
    }
    let base = baseline(servers);
    let rows: Vec<_> = ENDPOINTS
        .iter()
        .enumerate()
        .map(|(endpoint, name)| {
            let values = summarize(&samples[endpoint]);
            // A single-member proxy answers a tag list by asking its upstream, every time; that row
            // is an upstream round trip, not the registry serving something it holds.
            let build = if name.starts_with("tag list") { network_row } else { row };
            build(name, &values, base, Metric::Seconds, Absent::Failed)
        })
        .collect();
    publish(
        &table_name("image-endpoints"),
        table(
            "one warm request to each served endpoint; an empty cell is an endpoint the registry does not offer",
            servers,
            base,
            rows,
        ),
    )
}

/// Time one warm request to each endpoint, `None` where the registry does not serve it.
async fn endpoint_round(base: &str, http: &reqwest::Client) -> anyhow::Result<Vec<Option<f64>>> {
    let (repo, tag) = split_tag(STRESS_IMAGE);
    // Warm the registry: a pull-through proxy caches on demand and a sync-based registry mirrors from
    // the manifest, so only a completed pull leaves every party holding the same image.
    let scratch = tempfile::tempdir()?;
    crane_pull(base, STRESS_IMAGE, &scratch.path().join("warm.tar")).await?;

    let (layer, _) = largest_layer(base, STRESS_IMAGE).await?;
    let manifest_url = v2_url(base, &format!("{repo}/manifests/{tag}"))?;
    let (digest, config) = manifest_identity(http, &manifest_url, repo).await?;
    let by_digest = v2_url(base, &format!("{repo}/manifests/{digest}"))?;
    let tags = v2_url(base, &format!("{repo}/tags/list"))?;
    let (config_blob, layer_blob) = (blob_url(base, repo, &config)?, blob_url(base, repo, &layer)?);

    // Pull both blobs into the store before anything is timed. A proxy answers `HEAD` for a blob it
    // does not hold with an upstream `HEAD` rather than a download, so probing `HEAD` first would
    // price a network round trip and call it a serving cost.
    request(http, &config_blob, Probe::get(""), repo).await?;
    request(http, &layer_blob, Probe::get(""), repo).await?;

    Ok(vec![
        probe(http, &version_url(base)?, Probe::get(""), repo).await,
        probe(http, &manifest_url, Probe::get(MANIFEST_ACCEPT), repo).await,
        probe(http, &manifest_url, Probe::head(MANIFEST_ACCEPT), repo).await,
        probe(http, &by_digest, Probe::get(MANIFEST_ACCEPT), repo).await,
        probe(http, &config_blob, Probe::head(""), repo).await,
        probe(http, &config_blob, Probe::get(""), repo).await,
        probe(http, &layer_blob, Probe::range("bytes=0-1048575"), repo).await,
        probe(http, &tags, Probe::get(""), repo).await,
        probe(http, &format!("{tags}?n=1"), Probe::get(""), repo).await,
    ])
}

/// The manifest's own digest and its config blob's digest, resolving a multi-arch index to this host.
async fn manifest_identity(http: &reqwest::Client, url: &str, repo: &str) -> anyhow::Result<(String, String)> {
    let (top, digest) = fetch_manifest(http, url, repo).await?;
    let Some(entries) = top["manifests"].as_array() else {
        let config = top["config"]["digest"].as_str().context("manifest has no config")?;
        return Ok((digest, config.to_owned()));
    };
    let child = entries
        .iter()
        .find(|entry| entry["platform"]["architecture"] == docker_arch() && entry["platform"]["os"] == "linux")
        .and_then(|entry| entry["digest"].as_str())
        .with_context(|| format!("{STRESS_IMAGE} has no linux/{} manifest", docker_arch()))?;
    let child_url = url
        .rsplit_once('/')
        .map(|(prefix, _)| format!("{prefix}/{child}"))
        .context("manifest url has no reference")?;
    let (manifest, _) = fetch_manifest(http, &child_url, repo).await?;
    let config = manifest["config"]["digest"]
        .as_str()
        .context("manifest has no config")?;
    Ok((child.to_owned(), config.to_owned()))
}

/// A manifest and the digest the registry attributes to it.
async fn fetch_manifest(http: &reqwest::Client, url: &str, repo: &str) -> anyhow::Result<(serde_json::Value, String)> {
    let mut response = http.get(url).header("Accept", MANIFEST_ACCEPT).send().await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let token = bearer_token(http, &response, repo).await?;
        response = http
            .get(url)
            .header("Accept", MANIFEST_ACCEPT)
            .bearer_auth(token)
            .send()
            .await?;
    }
    let response = response.error_for_status()?;
    let digest = response
        .headers()
        .get("docker-content-digest")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = response.text().await?;
    Ok((serde_json::from_str(&body)?, digest))
}

/// How one endpoint is asked for: the method, and the headers that select a representation or range.
#[derive(Clone, Copy)]
struct Probe {
    head: bool,
    accept: &'static str,
    range: &'static str,
}

impl Probe {
    const fn get(accept: &'static str) -> Self {
        Self {
            head: false,
            accept,
            range: "",
        }
    }

    const fn head(accept: &'static str) -> Self {
        Self {
            head: true,
            accept,
            range: "",
        }
    }

    const fn range(range: &'static str) -> Self {
        Self {
            head: false,
            accept: "",
            range,
        }
    }
}

/// The median warm latency of `PROBES` requests, or `None` when the registry does not serve `url`.
async fn probe(http: &reqwest::Client, url: &str, probe: Probe, repo: &str) -> Option<f64> {
    request(http, url, probe, repo).await.ok()?;
    let mut latencies = Vec::with_capacity(PROBES);
    for _ in 0..PROBES {
        let start = Instant::now();
        request(http, url, probe, repo).await.ok()?;
        latencies.push(start.elapsed().as_secs_f64());
    }
    Summary::of(&latencies).map(|summary| summary.median)
}

/// One request, reading the body so the timing covers the response rather than its headers, and
/// answering a `401` with the bearer token the challenge asks for.
async fn request(http: &reqwest::Client, url: &str, probe: Probe, repo: &str) -> anyhow::Result<()> {
    let build = |token: Option<String>| {
        let mut request = if probe.head { http.head(url) } else { http.get(url) };
        if !probe.accept.is_empty() {
            request = request.header("Accept", probe.accept);
        }
        if !probe.range.is_empty() {
            request = request.header("Range", probe.range);
        }
        match token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    };
    let response = build(None).send().await?;
    let response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let token = bearer_token(http, &response, repo).await?;
        build(Some(token)).send().await?
    } else {
        response
    };
    let response = response.error_for_status()?;
    let mut response = response;
    while response.chunk().await?.is_some() {}
    Ok(())
}
