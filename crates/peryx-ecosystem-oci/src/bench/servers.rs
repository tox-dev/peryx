use std::io::{BufRead as _, Write as _};
use std::ops::Deref;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, bail};

use super::BenchEnvironment;
use super::images::{FLEET_IMAGE, PULL_IMAGES, READINESS_IMAGE, STRESS_IMAGE};
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::servers::Server;

/// The upstream every proxy caches and `direct` pulls from, when no local mirror is set.
const UPSTREAM: &str = "https://registry-1.docker.io";

/// Docker Hub's manifest endpoint, used by `direct` and to resolve the stress layer, when no mirror
/// is set.
pub const DOCKERHUB: &str = "https://index.docker.io/";

/// The upstream to use in place of `hub` (a Docker Hub URL): the local mirror when one is set, else
/// `hub` unchanged.
#[must_use]
pub(super) fn upstream_for(environment: &BenchEnvironment, hub: &str) -> String {
    environment.mirror.clone().unwrap_or_else(|| hub.to_owned())
}

/// Whether the parties talk to Docker Hub directly (and so need its credentials) rather than a local
/// mirror that already holds the images.
const fn mirrored(environment: &BenchEnvironment) -> bool {
    environment.mirror.is_some()
}

/// Docker Hub credentials, but only when a party actually talks to Docker Hub; a local mirror serves
/// the already-cached images over plain HTTP with no auth.
pub(super) fn hub_credentials(environment: &BenchEnvironment) -> Option<(String, String)> {
    if mirrored(environment) {
        None
    } else {
        environment.credentials.clone()
    }
}

/// The upstream as `distribution` sees it from inside its container: a mirror published on the host's
/// loopback is reachable there only through `host.docker.internal`, never the container's own
/// `127.0.0.1`. Unchanged when the upstream is Docker Hub.
fn container_upstream(environment: &BenchEnvironment) -> String {
    upstream_for(environment, UPSTREAM).replace("127.0.0.1", "host.docker.internal")
}

/// The report table name for a workload: `base` for the against-Docker-Hub run, `base-mirror` for the
/// shielded run, so both variants sit side by side in one report.
#[must_use]
pub(super) fn table_name(environment: &BenchEnvironment, base: &str) -> String {
    if mirrored(environment) {
        format!("{base}-mirror")
    } else {
        base.to_owned()
    }
}

/// The `registry:2` image tag `distribution` runs from, and the pinned `zot` release.
const DISTRIBUTION_IMAGE: &str = "registry:2";
const ZOT_VERSION: &str = "2.1.2";

/// Log crane in to Docker Hub for the `direct` transfers, when credentials are set. The local
/// proxies authenticate to the upstream themselves; crane only needs it for the no-proxy baseline.
///
/// # Errors
/// Returns an error when crane rejects the credentials.
pub(super) fn login_crane(environment: &BenchEnvironment) -> anyhow::Result<()> {
    if mirrored(environment) {
        return Ok(());
    }
    let Some((user, token)) = &environment.credentials else {
        return Ok(());
    };
    let mut command = Command::new(&environment.tools.crane);
    command.args(["auth", "login", "index.docker.io", "-u", user, "--password-stdin"]);
    command.stdin(std::process::Stdio::piped());
    let mut process = command.spawn().context("crane did not start")?;
    let mut child = ChildGuard::new(&mut process, false);
    child
        .child_mut()
        .stdin
        .take()
        .context("crane stdin")?
        .write_all(token.as_bytes())?;
    let status = child.wait().context("crane auth login")?;
    if !status.success() {
        bail!("crane auth login to Docker Hub failed");
    }
    Ok(())
}

/// A local pull-through cache that removes upstream variance from comparison runs.
pub(super) struct Mirror {
    port: u16,
    url: String,
    docker: PathBuf,
}

impl Mirror {
    pub(super) fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        let _ = Command::new(&self.docker)
            .args(["rm", "--force", &mirror_container(self.port)])
            .output();
    }
}

/// Start the shared mirror, point every party at it, and seed it with this run's images.
///
/// # Errors
/// Returns an error when the mirror container cannot start or become ready, or an image cannot be
/// seeded into it.
pub(super) async fn start_mirror(environment: &BenchEnvironment) -> anyhow::Result<Mirror> {
    let port = mirror_port()?;
    let mut command = Command::new(&environment.tools.docker);
    command
        .args(["run", "--rm", "-d", "--name", &mirror_container(port)])
        .args(["-p", &format!("127.0.0.1:{port}:5000")])
        .args(["-e", &format!("REGISTRY_PROXY_REMOTEURL={UPSTREAM}")]);
    if let Some((user, token)) = &environment.credentials {
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
    let url = format!("http://127.0.0.1:{port}");
    let mirror = Mirror {
        port,
        url: url.clone(),
        docker: environment.tools.docker.clone(),
    };
    wait_for_container_event(
        environment,
        &mirror_container(port),
        "listening on",
        tokio::time::Instant::now() + environment.startup_timeout,
    )
    .await?;
    println!("[oci] seeding the local mirror");
    seed_mirror(environment, &url).await?;
    Ok(mirror)
}

/// Pull every image the run touches through the mirror once, so the measured rounds hit its cache.
async fn seed_mirror(environment: &BenchEnvironment, url: &str) -> anyhow::Result<()> {
    for image in [STRESS_IMAGE, FLEET_IMAGE]
        .into_iter()
        .chain(PULL_IMAGES.iter().copied())
    {
        let scratch = tempfile::tempdir()?;
        readiness_pull(environment, url, image, &scratch.path().join("seed.tar"))
            .await
            .with_context(|| format!("seeding {image} into the mirror"))?;
    }
    Ok(())
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
#[must_use]
pub fn api_root(base: &str) -> String {
    let Some((scheme, remainder)) = base.split_once("://") else {
        return format!("{}/v2/", base.trim_end_matches('/'));
    };
    format!("{scheme}://{}/v2/", remainder.split('/').next().unwrap_or_default())
}

/// The reference a client (crane) pulls: the base's host and path with the scheme stripped, then the
/// repository and tag.
#[must_use]
pub fn client_reference(base: &str, repo: &str) -> String {
    let host = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))
        .unwrap_or(base)
        .trim_end_matches('/');
    format!("{host}/{repo}")
}

/// Whether a client must be told to skip TLS: the local proxies serve plain HTTP, Docker Hub HTTPS.
#[must_use]
pub fn insecure(base: &str) -> bool {
    base.starts_with("http://")
}

type BaseUrl = Arc<dyn Fn(&BenchEnvironment, u16) -> String + Send + Sync>;
type CommandFactory = Arc<dyn Fn(&BenchEnvironment, &BenchmarkContext, u16, &Path) -> Command + Send + Sync>;
type Setup = Arc<dyn Fn(&BenchEnvironment, u16, &Path) -> anyhow::Result<()> + Send + Sync>;
type Teardown = Arc<dyn Fn(&BenchEnvironment, u16) + Send + Sync>;

pub(super) struct BenchServer {
    report: Server,
    base_url: BaseUrl,
    command: Option<CommandFactory>,
    setup: Option<Setup>,
    teardown: Option<Teardown>,
    ready_log: &'static str,
}

impl Deref for BenchServer {
    type Target = Server;

    fn deref(&self) -> &Self::Target {
        &self.report
    }
}

impl BenchServer {
    pub(super) async fn start(
        &self,
        environment: &BenchEnvironment,
        context: &BenchmarkContext,
        state: &Path,
    ) -> anyhow::Result<Active> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let url = (self.base_url)(environment, port);
        let Some(command) = &self.command else {
            return Ok(Active {
                url,
                process: None,
                capture_threads: Vec::new(),
                teardown: None,
                environment: environment.clone(),
                port,
            });
        };
        if let Some(setup) = &self.setup {
            setup(environment, port, state)?;
        }
        let log = state.join("server.log");
        let mut command = command(environment, context, port, state);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let mut process = command
            .spawn()
            .with_context(|| format!("{} did not start", self.name))?;
        let mut guard = ChildGuard::new(&mut process, true);
        let stdout = guard.child_mut().stdout.take().context("server stdout")?;
        let stderr = guard.child_mut().stderr.take().context("server stderr")?;
        let StartupCapture {
            receiver,
            threads: capture_threads,
        } = capture_startup(stdout, stderr, &log, self.ready_log)?;
        guard.release();
        let mut active = Active {
            url,
            process: Some(process),
            capture_threads,
            teardown: self.teardown.clone(),
            environment: environment.clone(),
            port,
        };
        let deadline = tokio::time::Instant::now() + environment.startup_timeout;
        if wait_for_startup(receiver, deadline).await.is_err() {
            if let Some(process) = active.process.as_mut()
                && let Some(status) = process.try_wait()?
            {
                bail!("{} exited before its startup event with {status}", self.name);
            }
            let tail = std::fs::read_to_string(&log).unwrap_or_default();
            bail!("{} did not emit its startup event; server log tail:\n{tail}", self.name);
        }
        let scratch = tempfile::tempdir()?;
        if let Err(error) = readiness_pull_until(
            environment,
            &active.url,
            READINESS_IMAGE,
            &scratch.path().join("readiness.tar"),
            deadline,
        )
        .await
        {
            if let Some(process) = active.process.as_mut()
                && let Some(status) = process.try_wait()?
            {
                bail!("{} exited before OCI pulls were ready with {status}", self.name);
            }
            let tail = std::fs::read_to_string(log).unwrap_or_default();
            bail!(
                "{} could not pull through {}: {error:#}; server log tail:\n{tail}",
                self.name,
                active.url
            );
        }
        if let Some(process) = active.process.as_mut()
            && let Some(status) = process.try_wait()?
        {
            bail!("{} exited after OCI pull readiness with {status}", self.name);
        }
        Ok(active)
    }
}

struct StartupCapture {
    receiver: tokio::sync::mpsc::Receiver<()>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

fn capture_startup(
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    log: &Path,
    marker: &'static str,
) -> anyhow::Result<StartupCapture> {
    let log = Arc::new(Mutex::new(std::fs::File::create(log)?));
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    Ok(StartupCapture {
        receiver,
        threads: vec![
            capture_stream(stdout, Arc::clone(&log), sender.clone(), marker),
            capture_stream(stderr, log, sender, marker),
        ],
    })
}

fn capture_stream(
    stream: impl std::io::Read + Send + 'static,
    log: Arc<Mutex<std::fs::File>>,
    sender: tokio::sync::mpsc::Sender<()>,
    marker: &'static str,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stream).lines().map_while(Result::ok) {
            if let Ok(mut log) = log.lock() {
                let _ = writeln!(log, "{line}");
            }
            if line.contains(marker) {
                let _ = sender.try_send(());
            }
        }
    })
}

async fn wait_for_startup(
    mut receiver: tokio::sync::mpsc::Receiver<()>,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    tokio::time::timeout_at(deadline, receiver.recv())
        .await
        .context("startup event timed out")?
        .context("process closed its output before startup")
}

async fn wait_for_container_event(
    environment: &BenchEnvironment,
    container: &str,
    marker: &'static str,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    let mut command = Command::new(&environment.tools.docker);
    command
        .args(["logs", "--follow", container])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = command.spawn().context("docker logs did not start")?;
    let mut guard = ChildGuard::new(&mut process, false);
    let scratch = tempfile::tempdir()?;
    let stdout = guard.child_mut().stdout.take().context("docker logs stdout")?;
    let stderr = guard.child_mut().stderr.take().context("docker logs stderr")?;
    let StartupCapture { receiver, threads } =
        capture_startup(stdout, stderr, &scratch.path().join("mirror.log"), marker)?;
    let result = wait_for_startup(receiver, deadline).await;
    guard.terminate();
    for thread in threads {
        let _ = thread.join();
    }
    result.context("the OCI mirror did not emit its startup event")
}

async fn readiness_pull(
    environment: &BenchEnvironment,
    base: &str,
    image: &str,
    destination: &Path,
) -> anyhow::Result<()> {
    readiness_pull_until(
        environment,
        base,
        image,
        destination,
        tokio::time::Instant::now() + environment.startup_timeout,
    )
    .await
}

async fn readiness_pull_until(
    environment: &BenchEnvironment,
    base: &str,
    image: &str,
    destination: &Path,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    tokio::time::timeout_at(deadline, pull_image(environment, base, image, destination))
        .await
        .with_context(|| format!("pulling {image} through {base} timed out"))?
}

pub(super) async fn pull_image(
    environment: &BenchEnvironment,
    base: &str,
    image: &str,
    destination: &Path,
) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new(&environment.tools.crane);
    command.arg("pull");
    if insecure(base) {
        command.arg("--insecure");
    }
    command
        .arg(client_reference(base, image))
        .arg(destination)
        .kill_on_drop(true);
    let output = command.output().await.context("crane did not start")?;
    if !output.status.success() {
        bail!(
            "crane pull {image} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

pub(super) struct Active {
    pub url: String,
    process: Option<Child>,
    capture_threads: Vec<std::thread::JoinHandle<()>>,
    teardown: Option<Teardown>,
    environment: BenchEnvironment,
    port: u16,
}

impl Drop for Active {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            terminate_child(&mut process, true);
        }
        for thread in std::mem::take(&mut self.capture_threads) {
            let _ = thread.join();
        }
        if let Some(teardown) = &self.teardown {
            teardown(&self.environment, self.port);
        }
    }
}

struct ChildGuard<'child> {
    child: &'child mut Child,
    process_group: bool,
}

impl<'child> ChildGuard<'child> {
    const fn new(child: &'child mut Child, process_group: bool) -> Self {
        Self { child, process_group }
    }

    const fn child_mut(&mut self) -> &mut Child {
        &mut *self.child
    }

    fn wait(mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child_mut().wait()?;
        std::mem::forget(self);
        Ok(status)
    }

    const fn release(self) {
        std::mem::forget(self);
    }

    fn terminate(mut self) {
        let process_group = self.process_group;
        terminate_child(self.child_mut(), process_group);
        std::mem::forget(self);
    }
}

impl Drop for ChildGuard<'_> {
    fn drop(&mut self) {
        terminate_child(&mut *self.child, self.process_group);
    }
}

fn terminate_child(process: &mut Child, process_group: bool) {
    if process_group {
        #[cfg(unix)]
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{}", process.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = process.kill();
    let _ = process.wait();
}

#[must_use]
pub(super) fn all() -> Vec<BenchServer> {
    vec![peryx(), direct(), distribution(), zot()]
}

pub(super) fn reports(servers: &[BenchServer]) -> Vec<Server> {
    servers
        .iter()
        .map(|server| Server {
            name: server.name,
            homepage: server.homepage,
            base_url: server.report.base_url,
            probe: server.report.probe,
            command: None,
            setup: None,
            teardown: None,
        })
        .collect()
}

fn peryx_base(port: u16) -> String {
    format!("http://127.0.0.1:{port}/dockerhub/")
}

fn local_base(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

fn docker_hub_base(_: u16) -> String {
    DOCKERHUB.to_owned()
}

fn peryx() -> BenchServer {
    BenchServer {
        report: Server {
            name: "peryx",
            homepage: "https://peryx.readthedocs.io/",
            base_url: peryx_base,
            probe: api_root,
            command: None,
            setup: None,
            teardown: None,
        },
        base_url: Arc::new(|_, port| peryx_base(port)),
        command: Some(Arc::new(|_, context, _, state| {
            let mut command = Command::new(context.peryx_binary());
            command.arg("serve").arg("--config").arg(state.join("peryx.toml"));
            command
        })),
        setup: Some(Arc::new(|environment, port, state| {
            let auth = hub_credentials(environment).map_or_else(String::new, |(user, token)| {
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
                 [[index.upstream]]\n\
                 name = \"primary\"\n\
                 url = \"{cached}\"\n\
                 {auth}",
                data = toml_string(&state.join("data")),
                cached = upstream_for(environment, UPSTREAM),
            );
            std::fs::write(state.join("peryx.toml"), config)?;
            Ok(())
        })),
        teardown: None,
        ready_log: "peryx listening",
    }
}

fn direct() -> BenchServer {
    BenchServer {
        report: Server {
            name: "direct",
            homepage: "https://hub.docker.com/",
            base_url: docker_hub_base,
            probe: api_root,
            command: None,
            setup: None,
            teardown: None,
        },
        base_url: Arc::new(|environment, _| upstream_for(environment, DOCKERHUB)),
        command: None,
        setup: None,
        teardown: None,
        ready_log: "",
    }
}

fn distribution() -> BenchServer {
    BenchServer {
        report: Server {
            name: "distribution",
            homepage: "https://distribution.github.io/distribution/",
            base_url: local_base,
            probe: api_root,
            command: None,
            setup: None,
            teardown: None,
        },
        base_url: Arc::new(|_, port| local_base(port)),
        command: Some(Arc::new(|environment, _, port, _| {
            let mut command = Command::new(&environment.tools.docker);
            command
                .args(["run", "--rm", "--name", &container(port)])
                .args(["-p", &format!("127.0.0.1:{port}:5000")])
                .args([
                    "-e",
                    &format!("REGISTRY_PROXY_REMOTEURL={}", container_upstream(environment)),
                ]);
            if let Some((user, token)) = hub_credentials(environment) {
                command
                    .args(["-e", &format!("REGISTRY_PROXY_USERNAME={user}")])
                    .args(["-e", &format!("REGISTRY_PROXY_PASSWORD={token}")]);
            }
            command.arg(DISTRIBUTION_IMAGE);
            command
        })),
        setup: None,
        teardown: Some(Arc::new(remove_container)),
        ready_log: "listening on",
    }
}

fn zot() -> BenchServer {
    BenchServer {
        report: Server {
            name: "zot",
            homepage: "https://zotregistry.dev/",
            base_url: local_base,
            probe: api_root,
            command: None,
            setup: None,
            teardown: None,
        },
        base_url: Arc::new(|_, port| local_base(port)),
        command: Some(Arc::new(|environment, _, _, state| {
            let mut command = Command::new(environment.cache.join("zot"));
            command.arg("serve").arg(state.join("zot.json"));
            command
        })),
        setup: Some(Arc::new(|environment, port, state| {
            ensure_zot(environment)?;
            let url = upstream_for(environment, UPSTREAM);
            let mut sync = serde_json::json!({
                "registries": [{
                    "urls": [url],
                    "onDemand": true,
                    "tlsVerify": !mirrored(environment),
                    "content": [{ "prefix": "**" }]
                }]
            });
            if let Some((user, token)) = hub_credentials(environment) {
                let creds = state.join("zot-creds.json");
                let credentials = serde_json::to_vec(&serde_json::json!({
                    "registry-1.docker.io": { "username": user, "password": token }
                }))?;
                std::fs::write(&creds, credentials)?;
                sync["credentialsFile"] = serde_json::json!(creds.to_string_lossy());
            }
            let config = serde_json::json!({
                "storage": { "rootDirectory": state.join("zot-data") },
                "http": { "address": "127.0.0.1", "port": port.to_string() },
                "log": { "level": "info" },
                "extensions": { "sync": sync }
            });
            std::fs::write(state.join("zot.json"), serde_json::to_vec_pretty(&config)?)?;
            Ok(())
        })),
        teardown: None,
        ready_log: "listening on",
    }
}

/// The container name a docker-run competitor uses, unique by port so teardown can target it.
fn container(port: u16) -> String {
    format!("peryx-bench-oci-{port}")
}

/// Force-remove a docker-run competitor's container; killing the `docker run` client detaches from
/// it rather than stopping it, so a leaked container would burn CPU during the next party's run.
fn remove_container(environment: &BenchEnvironment, port: u16) {
    let _ = Command::new(&environment.tools.docker)
        .args(["rm", "--force", &container(port)])
        .output();
}

fn target_for(os: &'static str, arch: &'static str) -> anyhow::Result<(&'static str, &'static str)> {
    let os = match os {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!("no zot binary for {other}"),
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => bail!("no zot binary for {other}"),
    };
    Ok((os, arch))
}

/// Fetch the `zot` registry binary from its release asset, once.
fn ensure_zot(environment: &BenchEnvironment) -> anyhow::Result<()> {
    let binary = environment.cache.join("zot");
    if binary.exists() {
        return Ok(());
    }
    let (os, arch) = target_for(std::env::consts::OS, std::env::consts::ARCH)?;
    let url = format!("https://github.com/project-zot/zot/releases/download/v{ZOT_VERSION}/zot-{os}-{arch}");
    println!("[oci] fetching zot {ZOT_VERSION} ({os}/{arch})");
    std::fs::create_dir_all(&environment.cache)?;
    download(environment, &url, &binary)?;
    make_executable(&binary)
}

fn download(environment: &BenchEnvironment, url: &str, into: &Path) -> anyhow::Result<()> {
    let mut command = Command::new(&environment.tools.curl);
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
fn make_executable(_: &Path) -> anyhow::Result<()> {
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

#[cfg(test)]
#[path = "../../tests/unit/bench/servers.rs"]
mod tests;
