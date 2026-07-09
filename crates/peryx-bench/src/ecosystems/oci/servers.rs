//! The OCI registries under test: how each starts, the base a client pulls from, and its docs.
//!
//! Every competitor runs as a pull-through cache of Docker Hub (peryx's `cached` role, the
//! `distribution` reference registry in `proxy` mode, and `zot` with on-demand `sync`), so the
//! tables compare like with like against `direct`, a pull straight from Docker Hub. `distribution`
//! runs from its official `registry:2` image (env-configured, so nothing to mount); `zot` runs from
//! the native binary its releases ship, fetched once into `target/bench-oci`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};

use super::images::{FLEET_IMAGE, PULL_IMAGES, STRESS_IMAGE};
use crate::report::{peryx_binary, repo_root};
use crate::servers::Server;

/// The upstream every proxy caches and `direct` pulls from, when no local mirror is set.
const UPSTREAM: &str = "https://registry-1.docker.io";

/// Docker Hub's manifest endpoint, used by `direct` and to resolve the stress layer, when no mirror
/// is set.
pub const DOCKERHUB: &str = "https://index.docker.io/";

/// A local mirror URL that stands in for Docker Hub across every party, or `None` for Docker Hub
/// itself. Set once per run so a multi-round comparison fetches from a warm local cache instead of
/// exhausting Docker Hub's hourly pull ceiling.
fn mirror_override() -> &'static std::sync::Mutex<Option<String>> {
    static OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Point every party at `url` (a local mirror) instead of Docker Hub, or clear the redirect.
pub fn set_mirror(url: Option<String>) {
    *mirror_override().lock().expect("mirror override lock is not poisoned") = url;
}

/// The upstream to use in place of `hub` (a Docker Hub URL): the local mirror when one is set, else
/// `hub` unchanged.
pub fn upstream_for(hub: &str) -> String {
    mirror_override()
        .lock()
        .expect("mirror override lock is not poisoned")
        .clone()
        .unwrap_or_else(|| hub.to_owned())
}

/// Whether the parties talk to Docker Hub directly (and so need its credentials) rather than a local
/// mirror that already holds the images.
fn mirrored() -> bool {
    mirror_override()
        .lock()
        .expect("mirror override lock is not poisoned")
        .is_some()
}

/// Docker Hub credentials, but only when a party actually talks to Docker Hub; a local mirror serves
/// the already-cached images over plain HTTP with no auth.
fn hub_credentials() -> Option<(String, String)> {
    (!mirrored()).then(credentials).flatten()
}

/// The upstream as `distribution` sees it from inside its container: a mirror published on the host's
/// loopback is reachable there only through `host.docker.internal`, never the container's own
/// `127.0.0.1`. Unchanged when the upstream is Docker Hub.
fn container_upstream() -> String {
    upstream_for(UPSTREAM).replace("127.0.0.1", "host.docker.internal")
}

/// The report table name for a workload: `base` for the against-Docker-Hub run, `base-mirror` for the
/// shielded run, so both variants sit side by side in one report.
pub fn table_name(base: &str) -> String {
    if mirrored() {
        format!("{base}-mirror")
    } else {
        base.to_owned()
    }
}

/// The `registry:2` image tag `distribution` runs from, and the pinned `zot` release.
const DISTRIBUTION_IMAGE: &str = "registry:2";
const ZOT_VERSION: &str = "2.1.2";

/// Every party the tables compare, `direct` being the no-proxy baseline.
pub fn all() -> Vec<Server> {
    vec![peryx(), direct(), distribution(), zot()]
}

/// Docker Hub credentials from the environment, when both are set. Authenticated pulls get a higher
/// rate ceiling than the anonymous 100/hour, so every proxy and crane pick these up when present.
pub fn credentials() -> Option<(String, String)> {
    let user = std::env::var("DOCKERHUB_USERNAME")
        .ok()
        .filter(|user| !user.is_empty())?;
    let token = std::env::var("DOCKERHUB_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())?;
    Some((user, token))
}

/// Log crane in to Docker Hub for the `direct` transfers, when credentials are set. The local
/// proxies authenticate to the upstream themselves; crane only needs it for the no-proxy baseline.
///
/// # Errors
/// Returns an error when crane rejects the credentials.
pub fn login_crane() -> anyhow::Result<()> {
    if mirrored() {
        return Ok(());
    }
    let Some((user, token)) = credentials() else {
        return Ok(());
    };
    let mut command = Command::new("crane");
    command.args(["auth", "login", "index.docker.io", "-u", &user, "--password-stdin"]);
    command.stdin(std::process::Stdio::piped());
    let mut child = command.spawn().context("crane did not start")?;
    child.stdin.take().context("crane stdin")?.write_all(token.as_bytes())?;
    let status = child.wait().context("crane auth login")?;
    if !status.success() {
        bail!("crane auth login to Docker Hub failed");
    }
    Ok(())
}

/// A local pull-through cache of Docker Hub shared by every party for one run. Seeded once with the
/// run's images (a handful of Docker Hub pulls, well under the hourly ceiling), it then answers every
/// proxy's and `direct`'s fetches from local disk, so a multi-round comparison never re-hits Docker
/// Hub. It isolates the servers from upstream network and rate limits, at the cost that cold rows now
/// price proxy overhead rather than a real Docker Hub fetch; it is opt-in for exactly that reason.
pub struct Mirror {
    port: u16,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        set_mirror(None);
        let _ = Command::new("docker")
            .args(["rm", "--force", &mirror_container(self.port)])
            .output();
    }
}

/// Start the shared mirror, point every party at it, and seed it with this run's images.
///
/// # Errors
/// Returns an error when the mirror container cannot start or become ready, or an image cannot be
/// seeded into it.
pub async fn start_mirror(http: &reqwest::Client) -> anyhow::Result<Mirror> {
    let port = mirror_port()?;
    let mut command = Command::new("docker");
    command
        .args(["run", "--rm", "-d", "--name", &mirror_container(port)])
        .args(["-p", &format!("127.0.0.1:{port}:5000")])
        .args(["-e", &format!("REGISTRY_PROXY_REMOTEURL={UPSTREAM}")]);
    if let Some((user, token)) = credentials() {
        command
            .args(["-e", &format!("REGISTRY_PROXY_USERNAME={user}")])
            .args(["-e", &format!("REGISTRY_PROXY_PASSWORD={token}")]);
    }
    let output = command
        .arg(DISTRIBUTION_IMAGE)
        .output()
        .context("docker did not start the mirror")?;
    if !output.status.success() {
        bail!(
            "starting the OCI mirror failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // Own the container from here so any later failure tears it down and clears the redirect.
    let mirror = Mirror { port };
    let url = format!("http://127.0.0.1:{port}");
    wait_for_mirror(&url, http).await?;
    set_mirror(Some(url.clone()));
    println!("[oci] seeding the local mirror");
    seed_mirror(&url).await?;
    Ok(mirror)
}

/// Poll the mirror's `/v2/` until it answers or the deadline passes.
async fn wait_for_mirror(url: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    let probe = format!("{url}/v2/");
    for _ in 0..200 {
        if http.get(&probe).send().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    bail!("the OCI mirror never became ready")
}

/// Pull every image the run touches through the mirror once, so the measured rounds hit its cache.
async fn seed_mirror(url: &str) -> anyhow::Result<()> {
    let host = url.strip_prefix("http://").unwrap_or(url);
    for image in [STRESS_IMAGE, FLEET_IMAGE]
        .into_iter()
        .chain(PULL_IMAGES.iter().copied())
    {
        seed_image(host, image).await?;
    }
    Ok(())
}

/// `crane pull` one image through the mirror into a throwaway, retried past the mirror's own warmup.
async fn seed_image(host: &str, image: &str) -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let dest = scratch.path().join("seed.tar");
    let reference = format!("{host}/{image}");
    let mut last = String::new();
    for _ in 0..5 {
        let output = tokio::process::Command::new("crane")
            .args(["pull", "--insecure", &reference])
            .arg(&dest)
            .output()
            .await
            .context("crane did not start")?;
        if output.status.success() {
            return Ok(());
        }
        last = String::from_utf8_lossy(&output.stderr).into_owned();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    bail!("seeding {image} into the mirror failed:\n{last}")
}

/// A free localhost port for the mirror to bind (bound then released, so docker can claim it).
fn mirror_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// The mirror container's name, distinct from the competitor containers.
fn mirror_container(port: u16) -> String {
    format!("peryx-bench-oci-mirror-{port}")
}

/// The `/v2/` API root of a registry base: scheme and authority, then the distribution-spec path.
/// This doubles as the readiness probe: any HTTP status (Docker Hub answers `401`) means it is up.
pub fn api_root(base: &str) -> String {
    let url = url::Url::parse(base).expect("registry base is a valid URL");
    format!(
        "{}://{}/v2/",
        url.scheme(),
        url.host_str().map_or_else(String::new, |host| url
            .port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")))
    )
}

/// The reference a client (crane) pulls: the base's host and path with the scheme stripped, then the
/// repository and tag.
pub fn client_reference(base: &str, repo: &str) -> String {
    let host = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))
        .unwrap_or(base)
        .trim_end_matches('/');
    format!("{host}/{repo}")
}

/// Whether a client must be told to skip TLS: the local proxies serve plain HTTP, Docker Hub HTTPS.
pub fn insecure(base: &str) -> bool {
    base.starts_with("http://")
}

fn peryx() -> Server {
    Server {
        name: "peryx",
        homepage: "https://peryx.readthedocs.io/",
        base_url: |port| format!("http://127.0.0.1:{port}/dockerhub/"),
        probe: api_root,
        command: Some(|_port, state| {
            let mut command = Command::new(peryx_binary());
            command.arg("serve").arg("--config").arg(state.join("peryx.toml"));
            command
        }),
        setup: Some(|port, state| {
            let auth = hub_credentials().map_or_else(String::new, |(user, token)| {
                format!("username = {}\npassword = {}\n", toml_str(&user), toml_str(&token))
            });
            let config = format!(
                "host = \"127.0.0.1\"\n\
                 port = {port}\n\
                 data_dir = {data}\n\n\
                 [[index]]\n\
                 name = \"dockerhub\"\n\
                 route = \"dockerhub\"\n\
                 ecosystem = \"oci\"\n\
                 cached = \"{cached}\"\n\
                 {auth}",
                data = toml_string(&state.join("data")),
                cached = upstream_for(UPSTREAM),
            );
            std::fs::write(state.join("peryx.toml"), config)?;
            Ok(())
        }),
        teardown: None,
    }
}

fn direct() -> Server {
    Server {
        name: "direct",
        homepage: "https://hub.docker.com/",
        base_url: |_port| upstream_for(DOCKERHUB),
        probe: api_root,
        command: None,
        setup: None,
        teardown: None,
    }
}

fn distribution() -> Server {
    Server {
        name: "distribution",
        homepage: "https://distribution.github.io/distribution/",
        base_url: |port| format!("http://127.0.0.1:{port}/"),
        probe: api_root,
        command: Some(|port, _state| {
            let mut command = Command::new("docker");
            command
                .args(["run", "--rm", "--name", &container(port)])
                .args(["-p", &format!("127.0.0.1:{port}:5000")])
                .args(["-e", &format!("REGISTRY_PROXY_REMOTEURL={}", container_upstream())]);
            if let Some((user, token)) = hub_credentials() {
                command
                    .args(["-e", &format!("REGISTRY_PROXY_USERNAME={user}")])
                    .args(["-e", &format!("REGISTRY_PROXY_PASSWORD={token}")]);
            }
            command.arg(DISTRIBUTION_IMAGE);
            command
        }),
        setup: None,
        teardown: Some(remove_container),
    }
}

fn zot() -> Server {
    Server {
        name: "zot",
        homepage: "https://zotregistry.dev/",
        base_url: |port| format!("http://127.0.0.1:{port}/"),
        probe: api_root,
        command: Some(|_port, state| {
            let mut command = Command::new(bench_cache().join("zot"));
            command.arg("serve").arg(state.join("zot.json"));
            command
        }),
        setup: Some(|port, state| {
            ensure_zot()?;
            let url = upstream_for(UPSTREAM);
            let mut sync = serde_json::json!({
                "registries": [{
                    "urls": [url],
                    "onDemand": true,
                    "tlsVerify": !mirrored(),
                    "content": [{ "prefix": "**" }]
                }]
            });
            if let Some((user, token)) = hub_credentials() {
                let creds = state.join("zot-creds.json");
                std::fs::write(
                    &creds,
                    serde_json::to_vec(&serde_json::json!({
                        "registry-1.docker.io": { "username": user, "password": token }
                    }))?,
                )?;
                sync["credentialsFile"] = serde_json::json!(creds.to_string_lossy());
            }
            let config = serde_json::json!({
                "storage": { "rootDirectory": state.join("zot-data") },
                "http": { "address": "127.0.0.1", "port": port.to_string() },
                "log": { "level": "error" },
                "extensions": { "sync": sync }
            });
            std::fs::write(state.join("zot.json"), serde_json::to_vec_pretty(&config)?)?;
            Ok(())
        }),
        teardown: None,
    }
}

/// The container name a docker-run competitor uses, unique by port so teardown can target it.
fn container(port: u16) -> String {
    format!("peryx-bench-oci-{port}")
}

/// Force-remove a docker-run competitor's container; killing the `docker run` client detaches from
/// it rather than stopping it, so a leaked container would burn CPU during the next party's run.
fn remove_container(port: u16) {
    let _ = Command::new("docker")
        .args(["rm", "--force", &container(port)])
        .output();
}

/// The shared cache the zot binary is fetched into, beside the release build.
fn bench_cache() -> PathBuf {
    repo_root().join("target").join("bench-oci")
}

/// The host triple the release assets name, `os` from Docker's set and `arch` in amd64/arm64 terms.
fn host_target() -> anyhow::Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!("no zot binary for {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => bail!("no zot binary for {other}"),
    };
    Ok((os, arch))
}

/// Fetch the `zot` registry binary from its release asset, once.
fn ensure_zot() -> anyhow::Result<()> {
    let binary = bench_cache().join("zot");
    if binary.exists() {
        return Ok(());
    }
    let (os, arch) = host_target()?;
    let url = format!("https://github.com/project-zot/zot/releases/download/v{ZOT_VERSION}/zot-{os}-{arch}");
    println!("[oci] fetching zot {ZOT_VERSION} ({os}/{arch})");
    std::fs::create_dir_all(bench_cache())?;
    download(&url, &binary)?;
    make_executable(&binary)
}

fn download(url: &str, into: &Path) -> anyhow::Result<()> {
    let mut command = Command::new("curl");
    command
        .args(["--fail", "--location", "--silent", "--show-error", "--output"])
        .arg(into)
        .arg(url);
    let output = command.output().context("curl did not start")?;
    if !output.status.success() {
        bail!("downloading {url} failed:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(binary: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_binary: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// A path as a TOML basic string, backslashes and quotes escaped for the config we write.
fn toml_string(path: &Path) -> String {
    toml_str(&path.display().to_string())
}

/// A scalar as a TOML basic string, backslashes and quotes escaped.
fn toml_str(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
