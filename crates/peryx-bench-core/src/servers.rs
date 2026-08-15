//! The concrete servers under test and their index-URL shapes are per-ecosystem definitions; this
//! module only spawns, health-checks, and tears them down.

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use crate::context::BenchmarkContext;

/// How long a server gets to answer its first request (uvx may resolve an environment first).
const START_TIMEOUT: Duration = Duration::from_mins(3);

/// Override these deadlines for competitors that need setup before they can answer.
#[derive(Clone, Copy)]
pub struct StartupPolicy {
    pub timeout: Duration,
    pub request_timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for StartupPolicy {
    fn default() -> Self {
        Self {
            timeout: START_TIMEOUT,
            request_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(300),
        }
    }
}

/// # Errors
/// Returns an error when reqwest cannot build the client.
pub fn http_client() -> anyhow::Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder().build()?)
}

/// One index server under test; every field is filled in by a per-ecosystem definition.
pub struct Server {
    pub name: &'static str,
    pub homepage: &'static str,
    pub base_url: fn(u16) -> String,
    /// The readiness URL derived from the base, hit until any HTTP status answers.
    pub probe: fn(&str) -> String,
    pub command: Option<fn(&BenchmarkContext, u16, &Path) -> Command>,
    pub setup: Option<fn(u16, &Path) -> anyhow::Result<()>>,
    /// Teardown after the spawned process is killed, keyed by port. A container competitor detaches
    /// from the process that launched it, so killing that process is not enough; this removes it.
    pub teardown: Option<fn(u16)>,
}

/// A started server: where to reach it and the process behind it (none for direct).
pub struct Active {
    pub url: String,
    process: Option<Child>,
    log: Option<PathBuf>,
    probe_url: String,
    port: u16,
    teardown: Option<fn(u16)>,
}

impl Active {
    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(Child::id)
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            // gunicorn forks workers, and a `uvx` shim execs its payload: killing the direct child
            // orphans the rest, which then linger holding CPU and skewing every later measurement.
            // The child leads its own process group (see `start`), so signal the whole group.
            kill_process_group(process.id());
            let _ = process.kill();
            let _ = process.wait();
        }
        if let Some(teardown) = self.teardown {
            teardown(self.port);
        }
    }
}

fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    let _ = pid;
}

impl Server {
    /// Start this server against `state` and wait until it answers.
    ///
    /// # Errors
    /// Returns an error when the server exits early or never becomes ready; includes its log tail.
    pub async fn start(
        &self,
        context: &BenchmarkContext,
        state: &Path,
        client: &reqwest::Client,
    ) -> anyhow::Result<Active> {
        self.start_with_policy(context, state, client, StartupPolicy::default())
            .await
    }

    /// # Errors
    /// Returns an error when setup, process startup, or readiness fails.
    pub async fn start_with_policy(
        &self,
        context: &BenchmarkContext,
        state: &Path,
        client: &reqwest::Client,
        policy: StartupPolicy,
    ) -> anyhow::Result<Active> {
        let port = free_port()?;
        let url = (self.base_url)(port);
        let probe_url = (self.probe)(&url);
        let Some(command) = self.command else {
            return Ok(Active {
                url,
                process: None,
                log: None,
                probe_url,
                port,
                teardown: None,
            });
        };
        if let Some(setup) = self.setup {
            setup(port, state)?;
        }
        let log = state.join("server.log");
        let sink = std::fs::File::create(&log)?;
        let mut spawned = command(context, port, state);
        spawned.stdout(Stdio::from(sink.try_clone()?)).stderr(Stdio::from(sink));
        // Lead a fresh process group so teardown can reap forked workers along with the parent.
        #[cfg(unix)]
        spawned.process_group(0);
        let process = spawned
            .spawn()
            .with_context(|| format!("{} did not start", self.name))?;
        let mut active = Active {
            url,
            process: Some(process),
            log: Some(log),
            probe_url,
            port,
            teardown: self.teardown,
        };
        active.wait_ready(client, policy).await.with_context(|| {
            let tail = active
                .log
                .as_ref()
                .and_then(|log| std::fs::read_to_string(log).ok())
                .unwrap_or_default();
            format!("{}; server log tail:\n{}", self.name, last_chars(&tail, 2000))
        })?;
        Ok(active)
    }
}

impl Active {
    async fn wait_ready(&mut self, client: &reqwest::Client, policy: StartupPolicy) -> anyhow::Result<()> {
        let probe = self.probe_url.clone();
        let deadline = Instant::now() + policy.timeout;
        while Instant::now() < deadline {
            if let Some(process) = self.process.as_mut()
                && let Some(status) = process.try_wait()?
            {
                bail!("server exited early with {status}");
            }
            // Any HTTP status means the server is up and routing; only transport errors retry.
            match client.get(&probe).timeout(policy.request_timeout).send().await {
                Ok(_) => return Ok(()),
                Err(_) => tokio::time::sleep(policy.poll_interval).await,
            }
        }
        bail!("server never answered at {probe}")
    }
}

fn free_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn last_chars(text: &str, count: usize) -> &str {
    let start = text.len().saturating_sub(count);
    let boundary = (start..text.len())
        .find(|&index| text.is_char_boundary(index))
        .unwrap_or(0);
    &text[boundary..]
}
